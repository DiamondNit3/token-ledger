use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use rust_decimal::Decimal;

use crate::config::create_private_dir_all;
use crate::model::{
    CanonicalEvent, Client, ClientCoverageSnapshot, CoverageEventBoundary, CoverageStatus,
    CoverageWindowStatus, EventProvenance, LedgerCoverageSnapshot, ObservationProvenance,
    PricingDimensions, ScanRunSnapshot, ScanWarning, SuccessfulSourceScan, UsageObservation,
    UsageQuality, UsageVector, WarningCodeCount, pseudonymous_id, pseudonymous_session_id,
};
use crate::reconcile::{
    ImportReceipt, ParsedReconciliationImport, ReconciliationCounters, ReconciliationImportRecord,
    ReconciliationRouting, StoredReconciliationBucket,
};

const SCHEMA_VERSION: i64 = 7;
const SCAN_HEARTBEAT_STALE_AFTER_SECS: i64 = 15 * 60;
const COMPLETED_SCAN_HISTORY_LIMIT: i64 = 256;
const PRIVACY_MIGRATION_BATCH_SIZE: i64 = 256;
const V6_PRIVACY_STATE_KEY: &str = "v6_privacy_migration";
const V6_PRIVACY_PENDING: &str = "cleanup_pending";
const V6_PRIVACY_COMPLETE: &str = "complete";
const V7_PRIVACY_BARRIER_KEY: &str = "v7_privacy_barrier";
const V7_PRIVACY_PENDING: &str = "cleanup_pending";
const V7_PRIVACY_COMPLETE: &str = "complete";
const CODEX_ALIAS_SCOPE_CANDIDATE_LIMIT: usize = 3;

pub struct Ledger {
    connection: Connection,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SourceCheckpoint {
    pub id: i64,
    pub client: Client,
    pub path: PathBuf,
    pub compressed: bool,
    pub file_size: u64,
    pub modified_ns: i64,
    pub checkpoint_offset: u64,
    pub checkpoint_line: u64,
    pub checkpoint_hash: String,
    pub head_hash: String,
    pub adapter_state: serde_json::Value,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct SourceUpdate<'a> {
    pub source_id: i64,
    pub reset_observations: bool,
    pub file_size: u64,
    pub modified_ns: i64,
    pub checkpoint_offset: u64,
    pub checkpoint_line: u64,
    pub checkpoint_hash: &'a str,
    pub head_hash: &'a str,
    pub adapter_state: &'a serde_json::Value,
    pub observations: &'a [UsageObservation],
    pub warnings: &'a [ScanWarning],
    pub scan_run_id: i64,
}

#[derive(Debug, Clone, Default)]
pub struct LedgerStats {
    pub sources: u64,
    pub observations: u64,
    pub canonical_events: u64,
    pub warnings: u64,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_v6_invalidation(path, false)
    }

    /// Opens a ledger while explicitly allowing the schema-v6 privacy barrier
    /// to invalidate cached accounting. Callers must obtain deliberate user
    /// consent first: history whose original source files are gone cannot be
    /// rebuilt after this migration.
    pub fn open_for_v6_privacy_migration(path: &Path) -> Result<Self> {
        Self::open_with_v6_invalidation(path, true)
    }

    fn open_with_v6_invalidation(path: &Path, allow_v6_invalidation: bool) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|value| !value.as_os_str().is_empty()) {
            create_private_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }
        prepare_private_database_file(path)?;
        let connection = Connection::open(path)
            .with_context(|| format!("failed to open ledger database {}", path.display()))?;
        harden_sqlite_files(path)?;
        let ledger =
            Self::from_connection(connection, path.to_path_buf(), "WAL", allow_v6_invalidation)?;
        harden_sqlite_files(path)?;
        Ok(ledger)
    }

    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().context("failed to open in-memory ledger")?;
        Self::from_connection(connection, PathBuf::from(":memory:"), "MEMORY", true)
    }

    fn from_connection(
        connection: Connection,
        path: PathBuf,
        journal_mode: &str,
        allow_v6_invalidation: bool,
    ) -> Result<Self> {
        connection.pragma_update(None, "journal_mode", journal_mode)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "busy_timeout", 5_000_i64)?;
        let mut ledger = Self { connection, path };
        ledger.migrate(allow_v6_invalidation)?;
        Ok(ledger)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&mut self, allow_v6_invalidation: bool) -> Result<()> {
        self.connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS source_files (
                id INTEGER PRIMARY KEY,
                client TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                compressed INTEGER NOT NULL DEFAULT 0,
                file_size INTEGER NOT NULL DEFAULT 0,
                modified_ns INTEGER NOT NULL DEFAULT 0,
                checkpoint_offset INTEGER NOT NULL DEFAULT 0,
                checkpoint_line INTEGER NOT NULL DEFAULT 0,
                checkpoint_hash TEXT NOT NULL DEFAULT '',
                head_hash TEXT NOT NULL DEFAULT '',
                adapter_state TEXT NOT NULL DEFAULT '{}',
                content_hash TEXT NOT NULL DEFAULT '',
                last_scan_at TEXT,
                last_status TEXT NOT NULL DEFAULT 'new',
                privacy_write_generation INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS usage_observations (
                id INTEGER PRIMARY KEY,
                source_file_id INTEGER NOT NULL REFERENCES source_files(id) ON DELETE CASCADE,
                event_key TEXT NOT NULL,
                client TEXT NOT NULL,
                session_id TEXT NOT NULL,
                provider_message_id TEXT,
                occurred_at_utc TEXT NOT NULL,
                raw_model TEXT NOT NULL,
                provider TEXT NOT NULL,
                input_tokens_total INTEGER NOT NULL,
                input_tokens_uncached INTEGER NOT NULL,
                input_tokens_cached INTEGER NOT NULL,
                cache_write_5m_tokens INTEGER NOT NULL,
                cache_write_1h_tokens INTEGER NOT NULL,
                cache_write_unknown_tokens INTEGER NOT NULL,
                output_tokens_total INTEGER NOT NULL,
                reasoning_output_tokens INTEGER NOT NULL,
                web_search_requests INTEGER NOT NULL,
                web_fetch_requests INTEGER NOT NULL,
                dimensions_json TEXT NOT NULL,
                quality_rank INTEGER NOT NULL,
                coverage TEXT NOT NULL,
                source_locator TEXT NOT NULL,
                parser_version TEXT NOT NULL,
                warnings_json TEXT NOT NULL,
                privacy_write_generation INTEGER NOT NULL DEFAULT 0,
                UNIQUE(source_file_id, event_key)
            );

            CREATE INDEX IF NOT EXISTS idx_usage_observations_event
                ON usage_observations(client, event_key);
            CREATE INDEX IF NOT EXISTS idx_usage_observations_time
                ON usage_observations(occurred_at_utc);
            CREATE INDEX IF NOT EXISTS idx_usage_observations_model
                ON usage_observations(raw_model);
            CREATE INDEX IF NOT EXISTS idx_usage_observations_session
                ON usage_observations(session_id);

            CREATE TABLE IF NOT EXISTS codex_event_identity_aliases (
                source_file_id INTEGER NOT NULL REFERENCES source_files(id) ON DELETE CASCADE,
                canonical_event_key TEXT NOT NULL,
                session_scope TEXT NOT NULL,
                source_locator TEXT NOT NULL,
                usage_event_index INTEGER NOT NULL,
                PRIMARY KEY(source_file_id, canonical_event_key, session_scope)
            );
            CREATE TABLE IF NOT EXISTS codex_event_identity_replays (
                source_file_id INTEGER PRIMARY KEY REFERENCES source_files(id) ON DELETE CASCADE,
                globally_anchored INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS scan_runs (
                id INTEGER PRIMARY KEY,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                heartbeat_at TEXT,
                as_of TEXT,
                mode TEXT NOT NULL,
                source_count INTEGER NOT NULL DEFAULT 0,
                observation_count INTEGER NOT NULL DEFAULT 0,
                warning_count INTEGER NOT NULL DEFAULT 0,
                active_or_volatile_source_count INTEGER NOT NULL DEFAULT 0,
                provisional INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'running'
            );

            CREATE TABLE IF NOT EXISTS scan_warnings (
                id INTEGER PRIMARY KEY,
                scan_run_id INTEGER NOT NULL REFERENCES scan_runs(id) ON DELETE CASCADE,
                source_file_id INTEGER REFERENCES source_files(id) ON DELETE CASCADE,
                code TEXT NOT NULL,
                message TEXT NOT NULL,
                locator TEXT,
                created_at TEXT NOT NULL,
                privacy_write_generation INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS reconciliation_imports (
                id INTEGER PRIMARY KEY,
                content_digest TEXT NOT NULL UNIQUE,
                source_kind TEXT NOT NULL,
                adapter TEXT NOT NULL,
                provider TEXT,
                imported_at TEXT NOT NULL,
                byte_count INTEGER NOT NULL,
                bucket_count INTEGER NOT NULL,
                privacy_write_generation INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS reconciliation_buckets (
                id INTEGER PRIMARY KEY,
                import_id INTEGER NOT NULL REFERENCES reconciliation_imports(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                source_kind TEXT NOT NULL,
                bucket_start_utc TEXT NOT NULL,
                bucket_end_utc TEXT NOT NULL,
                provider TEXT NOT NULL,
                model TEXT,
                request_count INTEGER,
                input_tokens_uncached INTEGER,
                input_tokens_cached INTEGER,
                cache_write_5m_tokens INTEGER,
                cache_write_1h_tokens INTEGER,
                cache_write_unknown_tokens INTEGER,
                output_tokens INTEGER,
                provider_metered_usd TEXT,
                inference_geo TEXT,
                service_tier TEXT,
                provider_route TEXT,
                UNIQUE(import_id, ordinal)
            );
            CREATE INDEX IF NOT EXISTS idx_reconciliation_buckets_time
                ON reconciliation_buckets(bucket_start_utc, bucket_end_utc);
            CREATE INDEX IF NOT EXISTS idx_reconciliation_buckets_match
                ON reconciliation_buckets(provider, model, source_kind);
            "#,
        )?;

        // secure_delete must be selected outside a transaction. The EXCLUSIVE
        // transaction is then acquired *before* reading schema_version and is
        // retained across every logical migration. A second upgrader therefore
        // observes the first upgrader's committed version instead of replaying
        // the same pseudonymization boundary.
        self.connection.pragma_update(None, "secure_delete", "ON")?;
        let mut needs_physical_cleanup = false;
        let mut cleanup_follower = false;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Exclusive)?;
        let existing: Option<i64> = transaction
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            None => {
                ensure_v6_storage_columns(&transaction)?;
                transaction.execute(
                    "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION.to_string()],
                )?;
                transaction.execute(
                    "INSERT INTO meta(key, value) VALUES (?1, ?2)",
                    params![V6_PRIVACY_STATE_KEY, V6_PRIVACY_COMPLETE],
                )?;
                transaction.execute(
                    "INSERT INTO meta(key, value) VALUES (?1, ?2)",
                    params![V7_PRIVACY_BARRIER_KEY, V7_PRIVACY_COMPLETE],
                )?;
            }
            Some(version @ 1..=6) => {
                let opened_at_v6 = version == 6;
                if opened_at_v6 && !allow_v6_invalidation {
                    anyhow::bail!(
                        "opening this v0.4.1-era ledger requires an explicit privacy migration. \
                         The migration permanently deletes cached observations, scan history, warnings, \
                         and reconciliation imports; history cannot be rebuilt when its original Claude \
                         Code or Codex source files are missing. Retain those source files and export or \
                         back up the v0.4.1 ledger first, then run `token-ledger migrate \
                         --accept-history-loss`"
                    );
                }
                let mut current = version;
                if current == 1 {
                    migrate_v1_to_v2(&transaction)?;
                    current = 2;
                }
                if current == 2 {
                    migrate_v2_to_v3(&transaction)?;
                    current = 3;
                }
                if current == 3 {
                    advance_schema_version(&transaction, 3, 4)?;
                    current = 4;
                }
                if current == 4 {
                    scrub_legacy_private_values(&transaction)?;
                    advance_schema_version(&transaction, 4, 5)?;
                    current = 5;
                }
                if current == 5 {
                    migrate_v5_to_v6(&transaction)?;
                    current = 6;
                }
                debug_assert_eq!(current, 6);
                migrate_v6_to_v7(&transaction, opened_at_v6)?;
                needs_physical_cleanup = true;
            }
            Some(SCHEMA_VERSION) => {
                ensure_v6_storage_columns(&transaction)?;
                let state = transaction
                    .query_row(
                        "SELECT value FROM meta WHERE key=?1",
                        [V7_PRIVACY_BARRIER_KEY],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                match state.as_deref() {
                    Some(V7_PRIVACY_COMPLETE) => {}
                    Some(V7_PRIVACY_PENDING) => {
                        needs_physical_cleanup = true;
                        cleanup_follower = true;
                    }
                    _ => anyhow::bail!("database schema v7 is missing its privacy barrier marker"),
                }
            }
            Some(version) => anyhow::bail!(
                "database schema version {version} is unsupported by this binary (expected {SCHEMA_VERSION})"
            ),
        }
        ensure_v6_identity_indexes(&transaction)?;
        install_privacy_write_guard_triggers(&transaction)?;
        install_schema_version_guard_triggers(&transaction)?;
        transaction.commit()?;

        if needs_physical_cleanup {
            if cleanup_follower {
                // The process that advanced the schema owns the first cleanup
                // attempt. A follower that observed an already-pending barrier
                // waits so two VACUUM/checkpoint loops do not contend in
                // lockstep. If the owner crashed or failed, this process still
                // takes over after the bounded grace period.
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            self.complete_v7_privacy_cleanup()?;
        }
        Ok(())
    }

    fn complete_v7_privacy_cleanup(&mut self) -> Result<()> {
        const CLEANUP_RETRY_ATTEMPTS: usize = 100;
        const CLEANUP_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
        const CLEANUP_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(25);

        // The normal five-second SQLite busy timeout would apply to every
        // checkpoint attempt and accidentally turn this bounded retry loop
        // into a many-minute wait behind one live WAL reader. Use a short
        // per-attempt timeout here, then restore the normal connection policy
        // on every exit path.
        self.connection.busy_timeout(CLEANUP_BUSY_TIMEOUT)?;
        let cleanup = (|| -> Result<()> {
            let mut physically_clean = false;
            for _ in 0..CLEANUP_RETRY_ATTEMPTS {
                let state = self
                    .connection
                    .query_row(
                        "SELECT value FROM meta WHERE key=?1",
                        [V7_PRIVACY_BARRIER_KEY],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if state.as_deref() == Some(V7_PRIVACY_COMPLETE) {
                    return Ok(());
                }
                if !wal_checkpoint_complete(&self.connection)? {
                    std::thread::sleep(CLEANUP_RETRY_DELAY);
                    continue;
                }
                match self.connection.execute_batch("VACUUM") {
                    Ok(()) => {}
                    Err(rusqlite::Error::SqliteFailure(error, _))
                        if matches!(
                            error.code,
                            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                        ) =>
                    {
                        std::thread::sleep(CLEANUP_RETRY_DELAY);
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
                if wal_checkpoint_complete(&self.connection)? {
                    physically_clean = true;
                    break;
                }
                std::thread::sleep(CLEANUP_RETRY_DELAY);
            }
            if !physically_clean {
                anyhow::bail!("privacy cleanup could not obtain a complete SQLite WAL checkpoint");
            }
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Exclusive)?;
            let version: i64 = transaction.query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )?;
            if version != SCHEMA_VERSION {
                anyhow::bail!(
                    "database schema changed during privacy cleanup (found {version}, expected {SCHEMA_VERSION})"
                );
            }
            ensure_v6_identity_indexes(&transaction)?;
            transaction.execute(
                r#"INSERT INTO meta(key, value) VALUES (?1, ?2)
                   ON CONFLICT(key) DO UPDATE SET value=excluded.value"#,
                params![V7_PRIVACY_BARRIER_KEY, V7_PRIVACY_COMPLETE],
            )?;
            transaction.commit()?;
            Ok(())
        })();
        let restore = self
            .connection
            .busy_timeout(std::time::Duration::from_secs(5));
        cleanup.and(restore.map_err(Into::into))
    }

    /// Atomically acquires the single-writer scan lease. A live lease is
    /// rejected; conservatively stale leases are marked abandoned before the
    /// new run is created. The transaction is IMMEDIATE so two contenders
    /// cannot both observe an empty lease.
    pub fn start_scan(&mut self, mode: &str) -> Result<i64> {
        let now = Utc::now();
        let stale_before = now - TimeDelta::seconds(SCAN_HEARTBEAT_STALE_AFTER_SECS);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let running = {
            let mut statement = transaction.prepare(
                "SELECT id, started_at, heartbeat_at FROM scan_runs WHERE status='running' ORDER BY id",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut stale_ids = Vec::new();
        for (id, started_at, heartbeat_at) in running {
            let lease_time = heartbeat_at.as_deref().unwrap_or(&started_at);
            // Malformed legacy timestamps are treated as live. Recovery must
            // prefer a temporary rejection over stealing an active writer.
            let lease_time = DateTime::parse_from_rfc3339(lease_time)
                .map(|value| value.with_timezone(&Utc))
                .unwrap_or(now);
            if lease_time >= stale_before {
                anyhow::bail!(
                    "another scan is already in progress; retry after it completes (stale leases are recovered automatically)"
                );
            }
            stale_ids.push(id);
        }

        let now_text = now.to_rfc3339();
        for id in stale_ids {
            transaction.execute(
                r#"UPDATE scan_runs
                   SET completed_at=?1,
                       as_of=COALESCE(as_of, heartbeat_at, started_at),
                       status='abandoned', provisional=1
                   WHERE id=?2 AND status='running'"#,
                params![now_text, id],
            )?;
        }
        transaction.execute(
            "INSERT INTO scan_runs(started_at, heartbeat_at, mode, provisional) VALUES (?1, ?1, ?2, 1)",
            params![now_text, sanitize_scan_mode(mode)],
        )?;
        let id = transaction.last_insert_rowid();
        transaction.commit()?;
        Ok(id)
    }

    /// Refreshes a scan lease. A zero-row update means the run was recovered
    /// as abandoned and the old writer must stop rather than write concurrently.
    pub fn heartbeat_scan(&self, id: i64) -> Result<()> {
        let updated = self.connection.execute(
            "UPDATE scan_runs SET heartbeat_at=?1 WHERE id=?2 AND status='running'",
            params![Utc::now().to_rfc3339(), id],
        )?;
        if updated != 1 {
            anyhow::bail!("scan lease is no longer active");
        }
        Ok(())
    }

    pub fn finish_scan(
        &self,
        id: i64,
        source_count: u64,
        observation_count: u64,
        warning_count: u64,
        status: &str,
    ) -> Result<()> {
        self.finish_scan_snapshot(
            id,
            source_count,
            observation_count,
            warning_count,
            status,
            0,
            status != "ok",
            Utc::now(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_scan_snapshot(
        &self,
        id: i64,
        source_count: u64,
        observation_count: u64,
        warning_count: u64,
        status: &str,
        active_or_volatile_source_count: u64,
        provisional: bool,
        as_of: DateTime<Utc>,
    ) -> Result<()> {
        let completed_at = Utc::now();
        let status = sanitize_scan_status(status);
        let updated = self.connection.execute(
            r#"UPDATE scan_runs
               SET completed_at=?1, heartbeat_at=?1, as_of=?2,
                   source_count=?3, observation_count=?4, warning_count=?5,
                   status=?6, active_or_volatile_source_count=?7, provisional=?8
               WHERE id=?9 AND status='running'"#,
            params![
                completed_at.to_rfc3339(),
                as_of.to_rfc3339(),
                to_i64(source_count)?,
                to_i64(observation_count)?,
                to_i64(warning_count)?,
                status,
                to_i64(active_or_volatile_source_count)?,
                i64::from(provisional),
                id,
            ],
        )?;
        if updated != 1 {
            anyhow::bail!("scan lease is no longer active");
        }
        // Scan runs and their cascaded warnings are diagnostics, not accounting
        // facts. Ordinary reports auto-refresh, so retaining them forever would
        // make a healthy long-lived ledger grow without bound. Prune after the
        // current run completes so even zero-source scans retain exactly the
        // newest bounded set.
        self.connection.execute(
            r#"DELETE FROM scan_runs
               WHERE status<>'running'
                 AND id NOT IN (
                     SELECT id FROM scan_runs
                     WHERE status<>'running'
                     ORDER BY id DESC
                     LIMIT ?1
                 )"#,
            [COMPLETED_SCAN_HISTORY_LIMIT],
        )?;
        Ok(())
    }

    pub fn source_checkpoint(&self, path: &Path) -> Result<Option<SourceCheckpoint>> {
        let path_key = source_storage_key(path);
        self.connection
            .query_row(
                r#"SELECT id, client, path, compressed, file_size, modified_ns,
                          checkpoint_offset, checkpoint_line, checkpoint_hash, head_hash,
                          adapter_state, content_hash
                   FROM source_files WHERE path=?1"#,
                [path_key],
                |row| {
                    let client_text: String = row.get(1)?;
                    let state_text: String = row.get(10)?;
                    Ok(SourceCheckpoint {
                        id: row.get(0)?,
                        client: parse_client_sql(&client_text)?,
                        path: PathBuf::from(row.get::<_, String>(2)?),
                        compressed: row.get::<_, i64>(3)? != 0,
                        file_size: from_i64(row.get(4)?)?,
                        modified_ns: row.get(5)?,
                        checkpoint_offset: from_i64(row.get(6)?)?,
                        checkpoint_line: from_i64(row.get(7)?)?,
                        checkpoint_hash: row.get(8)?,
                        head_hash: row.get(9)?,
                        adapter_state: serde_json::from_str(&state_text).unwrap_or_default(),
                        content_hash: row.get(11)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn ensure_source(&self, client: Client, path: &Path, compressed: bool) -> Result<i64> {
        let path_key = source_storage_key(path);
        self.connection.execute(
            "INSERT INTO source_files(client, path, compressed, privacy_write_generation) VALUES (?1, ?2, ?3, 1) ON CONFLICT(path) DO UPDATE SET client=excluded.client, compressed=excluded.compressed, privacy_write_generation=source_files.privacy_write_generation+1",
            params![client.as_str(), path_key, compressed as i64],
        )?;
        self.connection
            .query_row(
                "SELECT id FROM source_files WHERE path=?1",
                [path_key],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn apply_source_update(&mut self, update: SourceUpdate<'_>) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let lease_is_active: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM scan_runs WHERE id=?1 AND status='running')",
            [update.scan_run_id],
            |row| row.get(0),
        )?;
        if !lease_is_active {
            anyhow::bail!("scan lease is no longer active");
        }
        let source_client_text: String = transaction.query_row(
            "SELECT client FROM source_files WHERE id=?1",
            [update.source_id],
            |row| row.get(0),
        )?;
        let source_client = parse_client_sql(&source_client_text)?;
        let codex_identity_replay = if source_client == Client::OpenaiCodex {
            if update.reset_observations {
                transaction.execute(
                    r#"INSERT INTO codex_event_identity_replays(
                           source_file_id, globally_anchored
                       ) VALUES (?1, 0)
                       ON CONFLICT(source_file_id) DO NOTHING"#,
                    [update.source_id],
                )?;
            }
            transaction.query_row(
                r#"SELECT EXISTS(
                       SELECT 1 FROM codex_event_identity_replays
                       WHERE source_file_id=?1
                   )"#,
                [update.source_id],
                |row| row.get::<_, bool>(0),
            )?
        } else {
            false
        };
        if update.reset_observations {
            transaction.execute(
                "DELETE FROM usage_observations WHERE source_file_id=?1",
                [update.source_id],
            )?;
        }
        for observation in update.observations {
            let migrated_alias = if codex_identity_replay {
                migrated_codex_event_alias(&transaction, update.source_id, observation)?
            } else {
                None
            };
            upsert_observation(
                &transaction,
                update.source_id,
                observation,
                migrated_alias.as_deref(),
            )?;
        }
        for warning in update.warnings {
            let warning = sanitize_scan_warning(warning);
            transaction.execute(
                "INSERT INTO scan_warnings(scan_run_id, source_file_id, code, message, locator, created_at, privacy_write_generation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                params![
                    update.scan_run_id,
                    update.source_id,
                    warning.code,
                    warning.message,
                    warning.locator,
                    Utc::now().to_rfc3339()
                ],
            )?;
        }
        transaction.execute(
            r#"UPDATE source_files SET
                    file_size=?1, modified_ns=?2, checkpoint_offset=?3,
                    checkpoint_line=?4, checkpoint_hash=?5, head_hash=?6,
                    adapter_state=?7, last_scan_at=?8, last_status=?9,
                    privacy_write_generation=privacy_write_generation+1
                WHERE id=?10"#,
            params![
                to_i64(update.file_size)?,
                update.modified_ns,
                to_i64(update.checkpoint_offset)?,
                to_i64(update.checkpoint_line)?,
                sanitize_hex_digest(update.checkpoint_hash),
                sanitize_hex_digest(update.head_hash),
                serde_json::to_string(&sanitize_adapter_state(
                    source_client,
                    update.adapter_state,
                ))?,
                Utc::now().to_rfc3339(),
                if update.checkpoint_offset == update.file_size {
                    "ok"
                } else {
                    "partial"
                },
                update.source_id,
            ],
        )?;
        if source_client == Client::OpenaiCodex && update.checkpoint_offset == update.file_size {
            transaction.execute(
                "DELETE FROM codex_event_identity_replays WHERE source_file_id=?1",
                [update.source_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Store the exact physical-source digest after a source update commits.
    /// A crash between these writes only leaves an empty/mismatched digest,
    /// which forces a conservative rebuild on the next scan.
    pub fn update_source_content_hash(&self, source_id: i64, content_hash: &str) -> Result<()> {
        self.connection.execute(
            r#"UPDATE source_files SET content_hash=?1,
                    privacy_write_generation=privacy_write_generation+1
               WHERE id=?2"#,
            params![sanitize_hex_digest(content_hash), source_id],
        )?;
        Ok(())
    }

    pub fn record_scan_warning(
        &self,
        scan_run_id: i64,
        source_file_id: Option<i64>,
        warning: &ScanWarning,
    ) -> Result<()> {
        let warning = sanitize_scan_warning(warning);
        self.connection.execute(
            "INSERT INTO scan_warnings(scan_run_id, source_file_id, code, message, locator, created_at, privacy_write_generation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
            params![
                scan_run_id,
                source_file_id,
                warning.code,
                warning.message,
                warning.locator,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn canonical_events(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Vec<CanonicalEvent>> {
        let mut statement = self.connection.prepare(
            r#"
            WITH ranked AS (
                SELECT *,
                    ROW_NUMBER() OVER (
                        PARTITION BY client, event_key
                        ORDER BY
                            CASE coverage
                                WHEN 'complete_known' THEN 2
                                WHEN 'partial_known' THEN 1
                                ELSE 0 END DESC,
                            quality_rank DESC,
                            input_tokens_total DESC,
                            output_tokens_total DESC,
                            id DESC
                    ) AS canonical_rank
                FROM usage_observations
            ), aggregated AS (
                SELECT
                    client, event_key,
                    MIN(occurred_at_utc) AS occurred_at_utc,
                    CASE client
                        WHEN 'claude_code' THEN
                            MAX(input_tokens_uncached) +
                            MAX(input_tokens_cached) +
                            MAX(
                                MAX(
                                    cache_write_5m_tokens + cache_write_1h_tokens + cache_write_unknown_tokens
                                ),
                                MAX(cache_write_5m_tokens) + MAX(cache_write_1h_tokens)
                            )
                        ELSE MAX(input_tokens_total)
                    END AS input_tokens_total,
                    CASE client
                        WHEN 'openai_codex' THEN MAX(
                            0,
                            MAX(input_tokens_total) -
                            MAX(input_tokens_cached) -
                            MAX(
                                MAX(
                                    cache_write_5m_tokens + cache_write_1h_tokens + cache_write_unknown_tokens
                                ),
                                MAX(cache_write_5m_tokens) + MAX(cache_write_1h_tokens)
                            )
                        )
                        ELSE MAX(input_tokens_uncached)
                    END AS input_tokens_uncached,
                    MAX(input_tokens_cached) AS input_tokens_cached,
                    MAX(cache_write_5m_tokens) AS cache_write_5m_tokens,
                    MAX(cache_write_1h_tokens) AS cache_write_1h_tokens,
                    MAX(
                        0,
                        MAX(
                            MAX(
                                cache_write_5m_tokens + cache_write_1h_tokens + cache_write_unknown_tokens
                            ),
                            MAX(cache_write_5m_tokens) + MAX(cache_write_1h_tokens)
                        ) - MAX(cache_write_5m_tokens) - MAX(cache_write_1h_tokens)
                    ) AS cache_write_unknown_tokens,
                    MAX(output_tokens_total) AS output_tokens_total,
                    MAX(reasoning_output_tokens) AS reasoning_output_tokens,
                    MAX(web_search_requests) AS web_search_requests,
                    MAX(web_fetch_requests) AS web_fetch_requests,
                    MAX(quality_rank) AS quality_rank,
                    CASE MIN(CASE coverage
                        WHEN 'complete_known' THEN 0
                        WHEN 'partial_known' THEN 1
                        ELSE 2 END)
                        WHEN 0 THEN 'complete_known'
                        WHEN 1 THEN 'partial_known'
                        ELSE 'unknown' END AS coverage,
                    COUNT(DISTINCT source_file_id) AS source_count,
                    GROUP_CONCAT(warnings_json, '|||TL|||') AS warning_sets
                FROM usage_observations
                GROUP BY client, event_key
            ), canonical AS (
                SELECT
                    aggregated.client,
                    aggregated.event_key,
                    ranked.session_id,
                    ranked.provider_message_id,
                    aggregated.occurred_at_utc,
                    ranked.raw_model,
                    ranked.provider,
                    aggregated.input_tokens_total,
                    aggregated.input_tokens_uncached,
                    aggregated.input_tokens_cached,
                    aggregated.cache_write_5m_tokens,
                    aggregated.cache_write_1h_tokens,
                    aggregated.cache_write_unknown_tokens,
                    aggregated.output_tokens_total,
                    aggregated.reasoning_output_tokens,
                    aggregated.web_search_requests,
                    aggregated.web_fetch_requests,
                    ranked.dimensions_json,
                    aggregated.quality_rank,
                    aggregated.coverage,
                    aggregated.source_count,
                    aggregated.warning_sets
                FROM aggregated
                JOIN ranked
                  ON ranked.client = aggregated.client
                 AND ranked.event_key = aggregated.event_key
                 AND ranked.canonical_rank = 1
            )
            SELECT * FROM canonical
            WHERE (?1 IS NULL OR occurred_at_utc >= ?1)
              AND (?2 IS NULL OR occurred_at_utc < ?2)
            ORDER BY occurred_at_utc, client, event_key
            "#,
        )?;
        let start_text = start.map(|value| value.to_rfc3339());
        let end_text = end.map(|value| value.to_rfc3339());
        let rows = statement.query_map(params![start_text, end_text], canonical_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn event_by_id(&self, event_id: &str) -> Result<Option<CanonicalEvent>> {
        Ok(self
            .canonical_events(None, None)?
            .into_iter()
            .find(|event| event.event_id == event_id))
    }

    /// Returns ingestion coverage for one client without exposing source paths
    /// or transcript-derived warning messages.
    pub fn client_coverage(&self, client: Client) -> Result<ClientCoverageSnapshot> {
        let source_count: u64 = from_i64(self.connection.query_row(
            "SELECT COUNT(*) FROM source_files WHERE client=?1",
            [client.as_str()],
            |row| row.get(0),
        )?)?;
        let observation_count: u64 = from_i64(self.connection.query_row(
            "SELECT COUNT(*) FROM usage_observations WHERE client=?1",
            [client.as_str()],
            |row| row.get(0),
        )?)?;
        let canonical_event_count: u64 = from_i64(self.connection.query_row(
            "SELECT COUNT(*) FROM (SELECT event_key FROM usage_observations WHERE client=?1 GROUP BY event_key)",
            [client.as_str()],
            |row| row.get(0),
        )?)?;
        let warning_count: u64 = from_i64(self.connection.query_row(
            r#"SELECT COUNT(*)
               FROM scan_warnings AS warning
               JOIN source_files AS source ON source.id=warning.source_file_id
               WHERE source.client=?1
                 AND warning.scan_run_id=(SELECT id FROM scan_runs ORDER BY id DESC LIMIT 1)"#,
            [client.as_str()],
            |row| row.get(0),
        )?)?;

        let last_successful_source_scan = self
            .connection
            .query_row(
                r#"SELECT last_scan_at, last_status
                   FROM source_files
                   WHERE client=?1 AND last_status='ok' AND last_scan_at IS NOT NULL
                   ORDER BY last_scan_at DESC, id DESC
                   LIMIT 1"#,
                [client.as_str()],
                |row| {
                    let completed_at: String = row.get(0)?;
                    Ok(SuccessfulSourceScan {
                        completed_at: parse_datetime_sql(0, &completed_at)?,
                        status: sanitize_identifier(&row.get::<_, String>(1)?, 32, "unknown"),
                    })
                },
            )
            .optional()?;

        let window_status = if source_count == 0 {
            CoverageWindowStatus::NoSources
        } else if observation_count == 0 {
            CoverageWindowStatus::NoObservations
        } else {
            CoverageWindowStatus::ObservedWindow
        };

        Ok(ClientCoverageSnapshot {
            client,
            window_status,
            source_count,
            observation_count,
            canonical_event_count,
            warning_count,
            last_successful_source_scan,
            earliest_canonical_event: self.coverage_boundary(client, false)?,
            latest_canonical_event: self.coverage_boundary(client, true)?,
        })
    }

    /// Returns the latest global scan run. Scan runs currently span whichever
    /// clients the caller selected, so client-specific success is represented
    /// separately by `ClientCoverageSnapshot::last_successful_source_scan`.
    pub fn latest_scan(&self) -> Result<Option<ScanRunSnapshot>> {
        self.connection
            .query_row(
                r#"SELECT started_at, completed_at, as_of, mode, status,
                          source_count, observation_count, warning_count,
                          active_or_volatile_source_count, provisional
                   FROM scan_runs
                   ORDER BY id DESC
                   LIMIT 1"#,
                [],
                |row| {
                    let started_at: String = row.get(0)?;
                    let completed_at: Option<String> = row.get(1)?;
                    let as_of: Option<String> = row.get(2)?;
                    Ok(ScanRunSnapshot {
                        started_at: parse_datetime_sql(0, &started_at)?,
                        completed_at: completed_at
                            .as_deref()
                            .map(|value| parse_datetime_sql(1, value))
                            .transpose()?,
                        as_of: as_of
                            .as_deref()
                            .map(|value| parse_datetime_sql(2, value))
                            .transpose()?,
                        mode: sanitize_identifier(&row.get::<_, String>(3)?, 32, "unknown"),
                        status: sanitize_identifier(&row.get::<_, String>(4)?, 32, "unknown"),
                        source_count: from_i64(row.get(5)?)?,
                        observation_count: from_i64(row.get(6)?)?,
                        warning_count: from_i64(row.get(7)?)?,
                        active_or_volatile_source_count: from_i64(row.get(8)?)?,
                        provisional: row.get::<_, i64>(9)? != 0,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Groups warnings by sanitized code and by client when the warning has a
    /// source association. Unassociated discovery warnings use `client: None`.
    pub fn warning_code_counts(&self) -> Result<Vec<WarningCodeCount>> {
        let mut statement = self.connection.prepare(
            r#"SELECT source.client, warning.code, COUNT(*)
               FROM scan_warnings AS warning
               LEFT JOIN source_files AS source ON source.id=warning.source_file_id
               WHERE warning.scan_run_id=(SELECT id FROM scan_runs ORDER BY id DESC LIMIT 1)
               GROUP BY source.client, warning.code
               ORDER BY source.client, warning.code"#,
        )?;
        let rows = statement.query_map([], |row| {
            let client_text: Option<String> = row.get(0)?;
            let client = client_text.as_deref().map(parse_client_sql).transpose()?;
            Ok((
                client,
                sanitize_warning_code(&row.get::<_, String>(1)?),
                from_i64(row.get(2)?)?,
            ))
        })?;

        // Different unsafe codes can sanitize to the same safe code, so merge
        // after sanitization rather than leaking their original distinctions.
        let mut counts: Vec<WarningCodeCount> = Vec::new();
        for row in rows {
            let (client, code, count) = row?;
            if let Some(existing) = counts
                .iter_mut()
                .find(|value| value.client == client && value.code == code)
            {
                existing.count = existing.count.saturating_add(count);
            } else {
                counts.push(WarningCodeCount {
                    client,
                    code,
                    count,
                });
            }
        }
        counts.sort_by(|left, right| {
            left.client
                .map(Client::as_str)
                .cmp(&right.client.map(Client::as_str))
                .then_with(|| left.code.cmp(&right.code))
        });
        Ok(counts)
    }

    /// Returns a complete privacy-safe coverage envelope for all clients.
    pub fn coverage_snapshot(&self) -> Result<LedgerCoverageSnapshot> {
        let last_scan = self.latest_scan()?;
        Ok(LedgerCoverageSnapshot {
            generated_at: Utc::now(),
            as_of: last_scan.as_ref().and_then(|scan| scan.as_of),
            active_or_volatile_source_count: last_scan
                .as_ref()
                .map_or(0, |scan| scan.active_or_volatile_source_count),
            provisional: last_scan.as_ref().is_some_and(|scan| scan.provisional),
            last_scan,
            clients: Client::ALL
                .into_iter()
                .map(|client| self.client_coverage(client))
                .collect::<Result<Vec<_>>>()?,
            warning_counts: self.warning_code_counts()?,
        })
    }

    /// Returns all observations that contributed to a canonical event while
    /// withholding paths, session ids, provider message ids, dimensions, and
    /// transcript bodies.
    pub fn event_provenance(&self, event_id: &str) -> Result<Option<EventProvenance>> {
        let Some((client, event_key)) = self.event_identity(event_id)? else {
            return Ok(None);
        };

        let mut statement = self.connection.prepare(
            r#"SELECT observation.source_file_id, source.path,
                      observation.source_locator, observation.parser_version,
                      observation.occurred_at_utc, observation.raw_model,
                      observation.input_tokens_total,
                      observation.input_tokens_uncached,
                      observation.input_tokens_cached,
                      observation.cache_write_5m_tokens,
                      observation.cache_write_1h_tokens,
                      observation.cache_write_unknown_tokens,
                      observation.output_tokens_total,
                      observation.reasoning_output_tokens,
                      observation.web_search_requests,
                      observation.web_fetch_requests,
                      observation.quality_rank, observation.coverage
               FROM usage_observations AS observation
               JOIN source_files AS source ON source.id=observation.source_file_id
               WHERE observation.client=?1 AND observation.event_key=?2
               ORDER BY observation.occurred_at_utc, observation.source_file_id, observation.id"#,
        )?;
        let rows = statement.query_map(params![client.as_str(), event_key], |row| {
            let source_storage_key: String = row.get(1)?;
            let occurred_at: String = row.get(4)?;
            let coverage: String = row.get(17)?;
            Ok((
                row.get::<_, i64>(0)?,
                ObservationProvenance {
                    pseudonymous_source_id: pseudonymous_source_id(&source_storage_key),
                    source_locator: sanitize_source_locator(&row.get::<_, String>(2)?),
                    parser_version: sanitize_identifier(&row.get::<_, String>(3)?, 96, "unknown"),
                    occurred_at: parse_datetime_sql(4, &occurred_at)?,
                    raw_model: sanitize_identifier(&row.get::<_, String>(5)?, 128, "unknown"),
                    usage: UsageVector {
                        input_tokens_total: from_i64(row.get(6)?)?,
                        input_tokens_uncached: from_i64(row.get(7)?)?,
                        input_tokens_cached: from_i64(row.get(8)?)?,
                        cache_write_5m_tokens: from_i64(row.get(9)?)?,
                        cache_write_1h_tokens: from_i64(row.get(10)?)?,
                        cache_write_unknown_tokens: from_i64(row.get(11)?)?,
                        output_tokens_total: from_i64(row.get(12)?)?,
                        reasoning_output_tokens: from_i64(row.get(13)?)?,
                        web_search_requests: from_i64(row.get(14)?)?,
                        web_fetch_requests: from_i64(row.get(15)?)?,
                    },
                    quality: UsageQuality::from_rank(row.get(16)?),
                    coverage: parse_coverage(&coverage),
                },
            ))
        })?;

        let mut source_ids = HashSet::new();
        let mut observations = Vec::new();
        for row in rows {
            let (source_id, observation) = row?;
            source_ids.insert(source_id);
            observations.push(observation);
        }
        let observation_count = observations.len() as u64;
        Ok(Some(EventProvenance {
            event_id: event_id.to_string(),
            client,
            observation_count,
            deduplicated_observation_count: observation_count.saturating_sub(1),
            source_count: source_ids.len() as u64,
            observations,
        }))
    }

    fn coverage_boundary(
        &self,
        client: Client,
        latest: bool,
    ) -> Result<Option<CoverageEventBoundary>> {
        let direction = if latest { "DESC" } else { "ASC" };
        let query = format!(
            r#"SELECT event_key, MIN(occurred_at_utc) AS canonical_occurred_at
               FROM usage_observations
               WHERE client=?1
               GROUP BY event_key
               ORDER BY canonical_occurred_at {direction}, event_key {direction}
               LIMIT 1"#
        );
        self.connection
            .query_row(&query, [client.as_str()], |row| {
                let event_key: String = row.get(0)?;
                let occurred_at: String = row.get(1)?;
                Ok(CoverageEventBoundary {
                    // Schema v5 stores the canonical public event pseudonym in
                    // place of the provider-derived event key.
                    event_id: event_key,
                    occurred_at: parse_datetime_sql(1, &occurred_at)?,
                })
            })
            .optional()
            .map_err(Into::into)
    }

    fn event_identity(&self, event_id: &str) -> Result<Option<(Client, String)>> {
        self.connection
            .query_row(
                "SELECT client, event_key FROM usage_observations WHERE event_key=?1 LIMIT 1",
                [event_id],
                |row| {
                    let client_text: String = row.get(0)?;
                    Ok((parse_client_sql(&client_text)?, row.get::<_, String>(1)?))
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn stats(&self) -> Result<LedgerStats> {
        let sources = query_count(&self.connection, "SELECT COUNT(*) FROM source_files")?;
        let observations =
            query_count(&self.connection, "SELECT COUNT(*) FROM usage_observations")?;
        let warnings = query_count(&self.connection, "SELECT COUNT(*) FROM scan_warnings")?;
        let canonical_events = query_count(
            &self.connection,
            "SELECT COUNT(*) FROM (SELECT 1 FROM usage_observations GROUP BY client, event_key)",
        )?;
        Ok(LedgerStats {
            sources,
            observations,
            canonical_events,
            warnings,
        })
    }

    pub fn source_rows(&self) -> Result<Vec<SourceCheckpoint>> {
        let mut statement = self.connection.prepare(
            r#"SELECT id, client, path, compressed, file_size, modified_ns,
                      checkpoint_offset, checkpoint_line, checkpoint_hash, head_hash,
                      adapter_state, content_hash
               FROM source_files ORDER BY client, path"#,
        )?;
        let rows = statement.query_map([], |row| {
            let client_text: String = row.get(1)?;
            let state_text: String = row.get(10)?;
            Ok(SourceCheckpoint {
                id: row.get(0)?,
                client: parse_client_sql(&client_text)?,
                path: PathBuf::from(row.get::<_, String>(2)?),
                compressed: row.get::<_, i64>(3)? != 0,
                file_size: from_i64(row.get(4)?)?,
                modified_ns: row.get(5)?,
                checkpoint_offset: from_i64(row.get(6)?)?,
                checkpoint_line: from_i64(row.get(7)?)?,
                checkpoint_hash: row.get(8)?,
                head_hash: row.get(9)?,
                adapter_state: serde_json::from_str(&state_text).unwrap_or_default(),
                content_hash: row.get(11)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Stores one provider export as immutable evidence. Exact-byte reimports
    /// are idempotent by SHA-256 digest and never touch usage observations.
    pub fn store_reconciliation_import(
        &mut self,
        import: &ParsedReconciliationImport,
    ) -> Result<ImportReceipt> {
        let content_digest = sanitize_content_digest(&import.content_digest);
        let source_kind = sanitize_identifier(&import.source_kind, 64, "unknown_source_kind");
        let adapter = sanitize_identifier(&import.adapter, 64, "unknown_adapter");
        let provider = import
            .provider
            .as_deref()
            .map(|value| sanitize_identifier(value, 64, "unknown"));
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, Option<String>, i64)> = transaction
            .query_row(
                r#"SELECT source_kind, adapter, provider, bucket_count
                   FROM reconciliation_imports WHERE content_digest=?1"#,
                [&content_digest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((source_kind, adapter, provider, bucket_count)) = existing {
            transaction.commit()?;
            return Ok(ImportReceipt {
                content_digest,
                source_kind,
                adapter,
                provider,
                bucket_count: from_i64(bucket_count)?,
                imported: false,
                note: "identical content was already imported; no rows changed".to_string(),
            });
        }

        let imported_at = Utc::now().to_rfc3339();
        transaction.execute(
            r#"INSERT INTO reconciliation_imports(
                   content_digest, source_kind, adapter, provider, imported_at,
                   byte_count, bucket_count, privacy_write_generation
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)"#,
            params![
                &content_digest,
                &source_kind,
                &adapter,
                &provider,
                imported_at,
                to_i64(import.byte_count)?,
                to_i64(import.buckets.len() as u64)?,
            ],
        )?;
        let import_id = transaction.last_insert_rowid();
        for (ordinal, bucket) in import.buckets.iter().enumerate() {
            let bucket_source_kind =
                sanitize_identifier(&bucket.source_kind, 64, "unknown_source_kind");
            let bucket_provider = sanitize_identifier(&bucket.provider, 64, "unknown");
            let bucket_model = bucket.model.as_deref().map(sanitize_model);
            let inference_geo =
                sanitize_optional_identifier(bucket.routing.inference_geo.as_deref(), 64);
            let service_tier =
                sanitize_optional_identifier(bucket.routing.service_tier.as_deref(), 64);
            let provider_route =
                sanitize_optional_identifier(bucket.routing.provider_route.as_deref(), 64);
            transaction.execute(
                r#"INSERT INTO reconciliation_buckets(
                       import_id, ordinal, source_kind, bucket_start_utc,
                       bucket_end_utc, provider, model, request_count,
                       input_tokens_uncached, input_tokens_cached,
                       cache_write_5m_tokens, cache_write_1h_tokens,
                       cache_write_unknown_tokens, output_tokens,
                       provider_metered_usd, inference_geo, service_tier,
                       provider_route
                   ) VALUES (
                       ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                       ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18
                   )"#,
                params![
                    import_id,
                    to_i64(ordinal as u64)?,
                    bucket_source_kind,
                    bucket.bucket_start.to_rfc3339(),
                    bucket.bucket_end.to_rfc3339(),
                    bucket_provider,
                    bucket_model,
                    optional_i64(bucket.counters.request_count)?,
                    optional_i64(bucket.counters.input_tokens_uncached)?,
                    optional_i64(bucket.counters.input_tokens_cached)?,
                    optional_i64(bucket.counters.cache_write_5m_tokens)?,
                    optional_i64(bucket.counters.cache_write_1h_tokens)?,
                    optional_i64(bucket.counters.cache_write_unknown_tokens)?,
                    optional_i64(bucket.counters.output_tokens)?,
                    bucket.provider_metered_usd.map(|value| value.to_string()),
                    inference_geo,
                    service_tier,
                    provider_route,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(ImportReceipt {
            content_digest,
            source_kind,
            adapter,
            provider,
            bucket_count: import.buckets.len() as u64,
            imported: true,
            note: "provider evidence imported without modifying local observations".to_string(),
        })
    }

    pub fn reconciliation_imports(&self) -> Result<Vec<ReconciliationImportRecord>> {
        let mut statement = self.connection.prepare(
            r#"SELECT id, content_digest, source_kind, adapter, provider,
                      imported_at, byte_count, bucket_count
               FROM reconciliation_imports ORDER BY id"#,
        )?;
        let rows = statement.query_map([], |row| {
            let imported_at: String = row.get(5)?;
            Ok(ReconciliationImportRecord {
                id: row.get(0)?,
                content_digest: row.get(1)?,
                source_kind: row.get(2)?,
                adapter: row.get(3)?,
                provider: row.get(4)?,
                imported_at: parse_datetime_sql(5, &imported_at)?,
                byte_count: from_i64(row.get(6)?)?,
                bucket_count: from_i64(row.get(7)?)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn reconciliation_buckets(&self) -> Result<Vec<StoredReconciliationBucket>> {
        let mut statement = self.connection.prepare(
            r#"SELECT bucket.import_id, import.content_digest, import.imported_at,
                      bucket.source_kind, bucket.bucket_start_utc,
                      bucket.bucket_end_utc, bucket.provider, bucket.model,
                      bucket.request_count, bucket.input_tokens_uncached,
                      bucket.input_tokens_cached, bucket.cache_write_5m_tokens,
                      bucket.cache_write_1h_tokens,
                      bucket.cache_write_unknown_tokens, bucket.output_tokens,
                      bucket.provider_metered_usd, bucket.inference_geo,
                      bucket.service_tier, bucket.provider_route
               FROM reconciliation_buckets AS bucket
               JOIN reconciliation_imports AS import ON import.id=bucket.import_id
               ORDER BY bucket.bucket_start_utc, bucket.provider, bucket.model,
                        bucket.import_id, bucket.ordinal"#,
        )?;
        let rows = statement.query_map([], |row| {
            let imported_at: String = row.get(2)?;
            let bucket_start: String = row.get(4)?;
            let bucket_end: String = row.get(5)?;
            let metered: Option<String> = row.get(15)?;
            Ok(StoredReconciliationBucket {
                import_id: row.get(0)?,
                import_digest: row.get(1)?,
                imported_at: parse_datetime_sql(2, &imported_at)?,
                source_kind: row.get(3)?,
                bucket_start: parse_datetime_sql(4, &bucket_start)?,
                bucket_end: parse_datetime_sql(5, &bucket_end)?,
                provider: row.get(6)?,
                model: row.get(7)?,
                counters: ReconciliationCounters {
                    request_count: optional_u64_sql(row.get(8)?)?,
                    input_tokens_uncached: optional_u64_sql(row.get(9)?)?,
                    input_tokens_cached: optional_u64_sql(row.get(10)?)?,
                    cache_write_5m_tokens: optional_u64_sql(row.get(11)?)?,
                    cache_write_1h_tokens: optional_u64_sql(row.get(12)?)?,
                    cache_write_unknown_tokens: optional_u64_sql(row.get(13)?)?,
                    output_tokens: optional_u64_sql(row.get(14)?)?,
                },
                provider_metered_usd: metered
                    .as_deref()
                    .map(|value| parse_decimal_sql(15, value))
                    .transpose()?,
                routing: ReconciliationRouting {
                    inference_geo: row.get(16)?,
                    service_tier: row.get(17)?,
                    provider_route: row.get(18)?,
                },
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn purge(&mut self) -> Result<()> {
        // Purge is a privacy boundary, not merely a logical DELETE. Secure
        // deletion overwrites removed cells, VACUUM rebuilds the main database
        // without freelist remnants, and both checkpoints truncate WAL copies.
        self.connection.pragma_update(None, "secure_delete", "ON")?;
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM reconciliation_buckets", [])?;
        transaction.execute("DELETE FROM reconciliation_imports", [])?;
        transaction.execute("DELETE FROM scan_warnings", [])?;
        transaction.execute("DELETE FROM scan_runs", [])?;
        transaction.execute("DELETE FROM usage_observations", [])?;
        transaction.execute("DELETE FROM source_files", [])?;
        transaction.commit()?;
        self.connection.execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE);
             VACUUM;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )?;
        Ok(())
    }
}

fn advance_schema_version(transaction: &Transaction<'_>, expected: i64, next: i64) -> Result<()> {
    let changed = transaction.execute(
        r#"UPDATE meta SET value=?1
           WHERE key='schema_version' AND CAST(value AS INTEGER)=?2"#,
        params![next.to_string(), expected],
    )?;
    if changed != 1 {
        anyhow::bail!(
            "database schema changed during migration (expected {expected} before advancing to {next})"
        );
    }
    Ok(())
}

fn migrate_v1_to_v2(transaction: &Transaction<'_>) -> Result<()> {
    let source_paths = {
        let mut statement = transaction.prepare("SELECT id, path FROM source_files")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, path) in source_paths {
        transaction.execute(
            "UPDATE source_files SET path=?1 WHERE id=?2",
            params![source_storage_key(Path::new(&path)), id],
        )?;
    }

    let source_locators = {
        let mut statement =
            transaction.prepare("SELECT id, source_locator FROM usage_observations")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, locator) in source_locators {
        transaction.execute(
            "UPDATE usage_observations SET source_locator=?1 WHERE id=?2",
            params![sanitize_source_locator(&locator), id],
        )?;
    }
    transaction.execute(
        "DELETE FROM scan_warnings WHERE code IN ('discovery_failed', 'source_scan_failed')",
        [],
    )?;
    advance_schema_version(transaction, 1, 2)
}

fn migrate_v2_to_v3(transaction: &Transaction<'_>) -> Result<()> {
    let columns = table_columns(transaction, "scan_runs")?;
    for (column, declaration) in [
        ("heartbeat_at", "TEXT"),
        ("as_of", "TEXT"),
        (
            "active_or_volatile_source_count",
            "INTEGER NOT NULL DEFAULT 0",
        ),
        ("provisional", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !columns.contains(column) {
            transaction.execute_batch(&format!(
                "ALTER TABLE scan_runs ADD COLUMN {column} {declaration}"
            ))?;
        }
    }
    transaction.execute(
        "UPDATE scan_runs SET heartbeat_at=COALESCE(completed_at, started_at), as_of=completed_at, provisional=CASE WHEN status='ok' THEN 0 ELSE 1 END",
        [],
    )?;
    advance_schema_version(transaction, 2, 3)
}

fn migrate_v5_to_v6(transaction: &Transaction<'_>) -> Result<()> {
    drop_privacy_write_guard_triggers(transaction)?;
    ensure_v6_storage_columns(transaction)?;
    recreate_v6_identity_tables(transaction)?;
    backfill_codex_event_identity_aliases(transaction)?;
    rescrub_v5_private_values(transaction)?;
    advance_schema_version(transaction, 5, 6)?;
    transaction.execute(
        r#"INSERT INTO meta(key, value) VALUES (?1, ?2)
           ON CONFLICT(key) DO UPDATE SET value=excluded.value"#,
        params![V6_PRIVACY_STATE_KEY, V6_PRIVACY_PENDING],
    )?;
    Ok(())
}

fn migrate_v6_to_v7(transaction: &Transaction<'_>, invalidate_pre_barrier_v6: bool) -> Result<()> {
    ensure_v6_storage_columns(transaction)?;
    if invalidate_pre_barrier_v6 {
        // A schema-v6 completion marker cannot prove that a v0.3 writer did
        // not reintroduce raw identifiers after v0.4.0 scrubbed the database.
        // Hashing these mixed rows again would also double-hash legitimate v6
        // pseudonyms. Invalidate the identifier-bearing accounting cache and
        // let the next scan rebuild it from its authoritative local sources.
        transaction.execute("DELETE FROM reconciliation_buckets", [])?;
        transaction.execute("DELETE FROM reconciliation_imports", [])?;
        transaction.execute("DELETE FROM scan_warnings", [])?;
        transaction.execute("DELETE FROM scan_runs", [])?;
        transaction.execute("DELETE FROM usage_observations", [])?;
        transaction.execute("DELETE FROM source_files", [])?;
    }
    advance_schema_version(transaction, 6, 7)?;
    transaction.execute(
        r#"INSERT INTO meta(key, value) VALUES (?1, ?2)
           ON CONFLICT(key) DO UPDATE SET value=excluded.value"#,
        params![V7_PRIVACY_BARRIER_KEY, V7_PRIVACY_PENDING],
    )?;
    Ok(())
}

fn ensure_v6_storage_columns(transaction: &Transaction<'_>) -> Result<()> {
    let source_columns = table_columns(transaction, "source_files")?;
    if !source_columns.contains("privacy_write_generation") {
        transaction.execute_batch(
            "ALTER TABLE source_files ADD COLUMN privacy_write_generation INTEGER NOT NULL DEFAULT 0;
             UPDATE source_files SET privacy_write_generation=1;",
        )?;
    }
    if !source_columns.contains("content_hash") {
        transaction.execute_batch(
            "ALTER TABLE source_files ADD COLUMN content_hash TEXT NOT NULL DEFAULT '';",
        )?;
    }

    let observation_columns = table_columns(transaction, "usage_observations")?;
    if !observation_columns.contains("privacy_write_generation") {
        transaction.execute_batch(
            "ALTER TABLE usage_observations ADD COLUMN privacy_write_generation INTEGER NOT NULL DEFAULT 0;
             UPDATE usage_observations SET privacy_write_generation=1;",
        )?;
    }
    for table in ["scan_warnings", "reconciliation_imports"] {
        let columns = table_columns(transaction, table)?;
        if !columns.contains("privacy_write_generation") {
            transaction.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN privacy_write_generation INTEGER NOT NULL DEFAULT 0;
                 UPDATE {table} SET privacy_write_generation=1;"
            ))?;
        }
    }
    Ok(())
}

fn table_columns(transaction: &Transaction<'_>, table: &str) -> Result<HashSet<String>> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_info({table})"))?;
    statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()
        .map_err(Into::into)
}

fn drop_privacy_write_guard_triggers(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS guard_source_files_v6_insert;
         DROP TRIGGER IF EXISTS guard_source_files_v6_update;
         DROP TRIGGER IF EXISTS guard_usage_observations_v6_insert;
         DROP TRIGGER IF EXISTS guard_usage_observations_v6_update;
         DROP TRIGGER IF EXISTS guard_scan_warnings_v6_insert;
         DROP TRIGGER IF EXISTS guard_scan_warnings_v6_update;
         DROP TRIGGER IF EXISTS guard_reconciliation_imports_v6_insert;
         DROP TRIGGER IF EXISTS guard_reconciliation_imports_v6_update;",
    )?;
    Ok(())
}

fn install_privacy_write_guard_triggers(transaction: &Transaction<'_>) -> Result<()> {
    drop_privacy_write_guard_triggers(transaction)?;
    transaction.execute_batch(
        r#"
        CREATE TRIGGER guard_source_files_v6_insert
        BEFORE INSERT ON source_files
        WHEN CAST((SELECT value FROM meta WHERE key='schema_version') AS INTEGER)>=6
             AND NEW.privacy_write_generation<>1
        BEGIN
            SELECT RAISE(ABORT, 'legacy writer rejected by privacy generation guard');
        END;
        CREATE TRIGGER guard_source_files_v6_update
        BEFORE UPDATE ON source_files
        WHEN CAST((SELECT value FROM meta WHERE key='schema_version') AS INTEGER)>=6
             AND NEW.privacy_write_generation<>OLD.privacy_write_generation+1
        BEGIN
            SELECT RAISE(ABORT, 'legacy writer rejected by privacy generation guard');
        END;
        CREATE TRIGGER guard_usage_observations_v6_insert
        BEFORE INSERT ON usage_observations
        WHEN CAST((SELECT value FROM meta WHERE key='schema_version') AS INTEGER)>=6
             AND NEW.privacy_write_generation<>1
        BEGIN
            SELECT RAISE(ABORT, 'legacy writer rejected by privacy generation guard');
        END;
        CREATE TRIGGER guard_usage_observations_v6_update
        BEFORE UPDATE ON usage_observations
        WHEN CAST((SELECT value FROM meta WHERE key='schema_version') AS INTEGER)>=6
             AND NEW.privacy_write_generation<>OLD.privacy_write_generation+1
        BEGIN
            SELECT RAISE(ABORT, 'legacy writer rejected by privacy generation guard');
        END;
        CREATE TRIGGER guard_scan_warnings_v6_insert
        BEFORE INSERT ON scan_warnings
        WHEN CAST((SELECT value FROM meta WHERE key='schema_version') AS INTEGER)>=6
             AND NEW.privacy_write_generation<>1
        BEGIN
            SELECT RAISE(ABORT, 'legacy writer rejected by privacy generation guard');
        END;
        CREATE TRIGGER guard_scan_warnings_v6_update
        BEFORE UPDATE ON scan_warnings
        WHEN CAST((SELECT value FROM meta WHERE key='schema_version') AS INTEGER)>=6
             AND NEW.privacy_write_generation<>OLD.privacy_write_generation+1
        BEGIN
            SELECT RAISE(ABORT, 'legacy writer rejected by privacy generation guard');
        END;
        CREATE TRIGGER guard_reconciliation_imports_v6_insert
        BEFORE INSERT ON reconciliation_imports
        WHEN CAST((SELECT value FROM meta WHERE key='schema_version') AS INTEGER)>=6
             AND NEW.privacy_write_generation<>1
        BEGIN
            SELECT RAISE(ABORT, 'legacy writer rejected by privacy generation guard');
        END;
        CREATE TRIGGER guard_reconciliation_imports_v6_update
        BEFORE UPDATE ON reconciliation_imports
        WHEN CAST((SELECT value FROM meta WHERE key='schema_version') AS INTEGER)>=6
             AND NEW.privacy_write_generation<>OLD.privacy_write_generation+1
        BEGIN
            SELECT RAISE(ABORT, 'legacy writer rejected by privacy generation guard');
        END;
        "#,
    )?;
    Ok(())
}

fn install_schema_version_guard_triggers(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        r#"
        DROP TRIGGER IF EXISTS guard_schema_version_no_downgrade;
        DROP TRIGGER IF EXISTS guard_schema_version_no_delete;
        DROP TRIGGER IF EXISTS guard_schema_version_no_replace;
        CREATE TRIGGER guard_schema_version_no_downgrade
        BEFORE UPDATE ON meta
        WHEN OLD.key='schema_version'
             AND (
                 NEW.key<>'schema_version'
                 OR CAST(NEW.value AS INTEGER)<CAST(OLD.value AS INTEGER)
             )
        BEGIN
            SELECT RAISE(ABORT, 'database schema downgrade rejected');
        END;
        CREATE TRIGGER guard_schema_version_no_delete
        BEFORE DELETE ON meta
        WHEN OLD.key='schema_version'
        BEGIN
            SELECT RAISE(ABORT, 'database schema downgrade rejected');
        END;
        CREATE TRIGGER guard_schema_version_no_replace
        BEFORE INSERT ON meta
        WHEN NEW.key='schema_version'
             AND EXISTS(SELECT 1 FROM meta WHERE key='schema_version')
             AND CAST(NEW.value AS INTEGER)<CAST(
                 (SELECT value FROM meta WHERE key='schema_version') AS INTEGER
             )
        BEGIN
            SELECT RAISE(ABORT, 'database schema downgrade rejected');
        END;
        "#,
    )?;
    Ok(())
}

#[cfg(unix)]
fn prepare_private_database_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).mode(0o600);
    let file = options
        .open(path)
        .with_context(|| format!("failed to create database file {}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure database file {}", path.display()))
}

#[cfg(not(unix))]
fn prepare_private_database_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn harden_sqlite_files(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if path == Path::new(":memory:") {
        return Ok(());
    }
    for candidate in [
        path.to_path_buf(),
        sqlite_sidecar_path(path, "-wal"),
        sqlite_sidecar_path(path, "-shm"),
    ] {
        if candidate.try_exists().with_context(|| {
            format!(
                "failed to inspect sensitive database file {}",
                candidate.display()
            )
        })? {
            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o600))
                .with_context(|| {
                    format!("failed to secure database file {}", candidate.display())
                })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(not(unix))]
fn harden_sqlite_files(_path: &Path) -> Result<()> {
    Ok(())
}

fn upsert_observation(
    transaction: &Transaction<'_>,
    source_id: i64,
    observation: &UsageObservation,
    migrated_alias: Option<&str>,
) -> Result<()> {
    let usage = &observation.usage;
    let client = observation.client;
    let event_key = migrated_alias
        .filter(|value| is_private_id(value, "evt"))
        .map(str::to_owned)
        .unwrap_or_else(|| observation.canonical_event_id());
    let session_id = pseudonymous_session_id(client, &observation.session_id);
    let provider_message_id = observation
        .provider_message_id
        .as_deref()
        .map(|value| private_provider_message_id(client, value));
    let dimensions =
        sanitize_dimensions_for_storage(client, &observation.dimensions, &observation.warnings);
    let warnings = sanitize_observation_warnings(&observation.warnings);
    let raw_model = sanitize_model(&observation.raw_model);
    let provider = sanitize_identifier(&observation.provider, 64, "unknown");
    let parser_version = sanitize_identifier(&observation.parser_version, 96, "unknown");
    transaction.execute(
        r#"INSERT INTO usage_observations(
                source_file_id, event_key, client, session_id, provider_message_id,
                occurred_at_utc, raw_model, provider,
                input_tokens_total, input_tokens_uncached, input_tokens_cached,
                cache_write_5m_tokens, cache_write_1h_tokens, cache_write_unknown_tokens,
                output_tokens_total, reasoning_output_tokens,
                web_search_requests, web_fetch_requests,
                dimensions_json, quality_rank, coverage,
                source_locator, parser_version, warnings_json,
                privacy_write_generation
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                ?19, ?20, ?21, ?22, ?23, ?24, 1
            )
            ON CONFLICT(source_file_id, event_key) DO UPDATE SET
                privacy_write_generation = usage_observations.privacy_write_generation + 1,
                session_id = CASE WHEN usage_observations.session_id='' THEN excluded.session_id ELSE usage_observations.session_id END,
                provider_message_id = COALESCE(excluded.provider_message_id, usage_observations.provider_message_id),
                occurred_at_utc = MIN(usage_observations.occurred_at_utc, excluded.occurred_at_utc),
                raw_model = CASE WHEN usage_observations.raw_model='' OR usage_observations.raw_model='unknown' THEN excluded.raw_model ELSE usage_observations.raw_model END,
                provider = CASE WHEN usage_observations.provider='' OR usage_observations.provider='unknown' THEN excluded.provider ELSE usage_observations.provider END,
                input_tokens_total = CASE usage_observations.client
                    WHEN 'claude_code' THEN
                        MAX(usage_observations.input_tokens_uncached, excluded.input_tokens_uncached) +
                        MAX(usage_observations.input_tokens_cached, excluded.input_tokens_cached) +
                        MAX(
                            usage_observations.cache_write_5m_tokens + usage_observations.cache_write_1h_tokens + usage_observations.cache_write_unknown_tokens,
                            excluded.cache_write_5m_tokens + excluded.cache_write_1h_tokens + excluded.cache_write_unknown_tokens,
                            MAX(usage_observations.cache_write_5m_tokens, excluded.cache_write_5m_tokens) +
                                MAX(usage_observations.cache_write_1h_tokens, excluded.cache_write_1h_tokens)
                        )
                    ELSE MAX(usage_observations.input_tokens_total, excluded.input_tokens_total)
                END,
                input_tokens_uncached = CASE usage_observations.client
                    WHEN 'openai_codex' THEN MAX(
                        0,
                        MAX(usage_observations.input_tokens_total, excluded.input_tokens_total) -
                        MAX(usage_observations.input_tokens_cached, excluded.input_tokens_cached) -
                        MAX(
                            usage_observations.cache_write_5m_tokens + usage_observations.cache_write_1h_tokens + usage_observations.cache_write_unknown_tokens,
                            excluded.cache_write_5m_tokens + excluded.cache_write_1h_tokens + excluded.cache_write_unknown_tokens,
                            MAX(usage_observations.cache_write_5m_tokens, excluded.cache_write_5m_tokens) +
                                MAX(usage_observations.cache_write_1h_tokens, excluded.cache_write_1h_tokens)
                        )
                    )
                    ELSE MAX(usage_observations.input_tokens_uncached, excluded.input_tokens_uncached)
                END,
                input_tokens_cached = MAX(usage_observations.input_tokens_cached, excluded.input_tokens_cached),
                cache_write_5m_tokens = MAX(usage_observations.cache_write_5m_tokens, excluded.cache_write_5m_tokens),
                cache_write_1h_tokens = MAX(usage_observations.cache_write_1h_tokens, excluded.cache_write_1h_tokens),
                cache_write_unknown_tokens = MAX(
                    0,
                    MAX(
                        usage_observations.cache_write_5m_tokens + usage_observations.cache_write_1h_tokens + usage_observations.cache_write_unknown_tokens,
                        excluded.cache_write_5m_tokens + excluded.cache_write_1h_tokens + excluded.cache_write_unknown_tokens,
                        MAX(usage_observations.cache_write_5m_tokens, excluded.cache_write_5m_tokens) +
                            MAX(usage_observations.cache_write_1h_tokens, excluded.cache_write_1h_tokens)
                    ) -
                    MAX(usage_observations.cache_write_5m_tokens, excluded.cache_write_5m_tokens) -
                    MAX(usage_observations.cache_write_1h_tokens, excluded.cache_write_1h_tokens)
                ),
                output_tokens_total = MAX(usage_observations.output_tokens_total, excluded.output_tokens_total),
                reasoning_output_tokens = MAX(usage_observations.reasoning_output_tokens, excluded.reasoning_output_tokens),
                web_search_requests = MAX(usage_observations.web_search_requests, excluded.web_search_requests),
                web_fetch_requests = MAX(usage_observations.web_fetch_requests, excluded.web_fetch_requests),
                dimensions_json = CASE
                    WHEN (CASE excluded.coverage WHEN 'complete_known' THEN 2 WHEN 'partial_known' THEN 1 ELSE 0 END) >=
                         (CASE usage_observations.coverage WHEN 'complete_known' THEN 2 WHEN 'partial_known' THEN 1 ELSE 0 END)
                    THEN excluded.dimensions_json
                    ELSE usage_observations.dimensions_json
                END,
                quality_rank = MAX(usage_observations.quality_rank, excluded.quality_rank),
                coverage = CASE
                    WHEN usage_observations.coverage='complete_known' OR excluded.coverage='complete_known' THEN 'complete_known'
                    WHEN usage_observations.coverage='partial_known' OR excluded.coverage='partial_known' THEN 'partial_known'
                    ELSE 'unknown'
                END,
                source_locator = CASE
                    WHEN (CASE excluded.coverage WHEN 'complete_known' THEN 2 WHEN 'partial_known' THEN 1 ELSE 0 END) >=
                         (CASE usage_observations.coverage WHEN 'complete_known' THEN 2 WHEN 'partial_known' THEN 1 ELSE 0 END)
                    THEN excluded.source_locator
                    ELSE usage_observations.source_locator
                END,
                parser_version = CASE
                    WHEN (CASE excluded.coverage WHEN 'complete_known' THEN 2 WHEN 'partial_known' THEN 1 ELSE 0 END) >=
                         (CASE usage_observations.coverage WHEN 'complete_known' THEN 2 WHEN 'partial_known' THEN 1 ELSE 0 END)
                    THEN excluded.parser_version
                    ELSE usage_observations.parser_version
                END,
                warnings_json = CASE
                    WHEN (CASE excluded.coverage WHEN 'complete_known' THEN 2 WHEN 'partial_known' THEN 1 ELSE 0 END) >=
                         (CASE usage_observations.coverage WHEN 'complete_known' THEN 2 WHEN 'partial_known' THEN 1 ELSE 0 END)
                    THEN excluded.warnings_json
                    ELSE usage_observations.warnings_json
                END"#,
        params![
            source_id,
            event_key,
            observation.client.as_str(),
            session_id,
            provider_message_id,
            observation.occurred_at.to_rfc3339(),
            raw_model,
            provider,
            to_i64(usage.input_tokens_total)?,
            to_i64(usage.input_tokens_uncached)?,
            to_i64(usage.input_tokens_cached)?,
            to_i64(usage.cache_write_5m_tokens)?,
            to_i64(usage.cache_write_1h_tokens)?,
            to_i64(usage.cache_write_unknown_tokens)?,
            to_i64(usage.output_tokens_total)?,
            to_i64(usage.reasoning_output_tokens)?,
            to_i64(usage.web_search_requests)?,
            to_i64(usage.web_fetch_requests)?,
            serde_json::to_string(&dimensions)?,
            observation.quality.rank(),
            observation.coverage.as_str(),
            sanitize_source_locator(&observation.source_locator),
            parser_version,
            serde_json::to_string(&warnings)?,
        ],
    )?;
    Ok(())
}

fn canonical_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CanonicalEvent> {
    let client_text: String = row.get(0)?;
    let event_key: String = row.get(1)?;
    let client = parse_client_sql(&client_text)?;
    let occurred_at_text: String = row.get(4)?;
    let dimensions_text: String = row.get(17)?;
    let coverage_text: String = row.get(19)?;
    let warning_sets: Option<String> = row.get(21)?;
    Ok(CanonicalEvent {
        event_id: event_key.clone(),
        event_key,
        client,
        session_id: row.get(2)?,
        provider_message_id: row.get(3)?,
        occurred_at: DateTime::parse_from_rfc3339(&occurred_at_text)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?
            .with_timezone(&Utc),
        raw_model: row.get(5)?,
        provider: row.get(6)?,
        usage: UsageVector {
            input_tokens_total: from_i64(row.get(7)?)?,
            input_tokens_uncached: from_i64(row.get(8)?)?,
            input_tokens_cached: from_i64(row.get(9)?)?,
            cache_write_5m_tokens: from_i64(row.get(10)?)?,
            cache_write_1h_tokens: from_i64(row.get(11)?)?,
            cache_write_unknown_tokens: from_i64(row.get(12)?)?,
            output_tokens_total: from_i64(row.get(13)?)?,
            reasoning_output_tokens: from_i64(row.get(14)?)?,
            web_search_requests: from_i64(row.get(15)?)?,
            web_fetch_requests: from_i64(row.get(16)?)?,
        },
        dimensions: serde_json::from_str(&dimensions_text)
            .unwrap_or_else(|_| PricingDimensions::default()),
        quality: UsageQuality::from_rank(row.get(18)?),
        coverage: parse_coverage(&coverage_text),
        source_count: from_i64(row.get(20)?)?,
        warnings: parse_warning_sets(warning_sets.as_deref()),
    })
}

fn parse_warning_sets(raw: Option<&str>) -> Vec<String> {
    let mut values = HashSet::new();
    for set in raw.unwrap_or_default().split("|||TL|||") {
        if let Ok(warnings) = serde_json::from_str::<Vec<String>>(set) {
            values.extend(warnings);
        }
    }
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort();
    values
}

fn scrub_legacy_private_values(transaction: &Transaction<'_>) -> Result<()> {
    let mut last_id = 0;
    loop {
        let ids = migration_id_batch(
            transaction,
            "SELECT id FROM usage_observations WHERE id>?1 ORDER BY id LIMIT ?2",
            last_id,
        )?;
        if ids.is_empty() {
            break;
        }
        for id in &ids {
            let row = transaction.query_row(
                r#"SELECT client, event_key, session_id, provider_message_id,
                          raw_model, provider, dimensions_json, source_locator,
                          parser_version, warnings_json
                   FROM usage_observations WHERE id=?1"#,
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )?;
            let (
                client_text,
                event_key,
                session_id,
                provider_message_id,
                raw_model,
                provider,
                dimensions_json,
                source_locator,
                parser_version,
                warnings_json,
            ) = row;
            let client = parse_client_sql(&client_text)?;
            let warnings = serde_json::from_str::<Vec<String>>(&warnings_json).unwrap_or_default();
            let dimensions = serde_json::from_str::<PricingDimensions>(&dimensions_json)
                .unwrap_or_else(|_| PricingDimensions::default());
            let dimensions = sanitize_dimensions_for_storage(client, &dimensions, &warnings);
            transaction.execute(
                r#"UPDATE usage_observations SET
                       event_key=?1, session_id=?2, provider_message_id=?3,
                       raw_model=?4, provider=?5, dimensions_json=?6,
                       source_locator=?7, parser_version=?8, warnings_json=?9
                   WHERE id=?10"#,
                params![
                    crate::model::stable_id(&[client.as_str(), &event_key]),
                    migrated_session_id(client, &session_id),
                    provider_message_id
                        .as_deref()
                        .map(|value| private_provider_message_id(client, value)),
                    sanitize_model(&raw_model),
                    sanitize_identifier(&provider, 64, "unknown"),
                    serde_json::to_string(&dimensions)?,
                    sanitize_source_locator(&source_locator),
                    sanitize_identifier(&parser_version, 96, "unknown"),
                    serde_json::to_string(&sanitize_observation_warnings(&warnings))?,
                    id,
                ],
            )?;
        }
        last_id = *ids.last().expect("non-empty migration batch");
    }

    let mut last_id = 0;
    loop {
        let ids = migration_id_batch(
            transaction,
            "SELECT id FROM source_files WHERE id>?1 ORDER BY id LIMIT ?2",
            last_id,
        )?;
        if ids.is_empty() {
            break;
        }
        for id in &ids {
            let (client_text, path, checkpoint_hash, head_hash, state_json, last_status) =
                transaction.query_row(
                    r#"SELECT client, path, checkpoint_hash, head_hash,
                              adapter_state, last_status
                       FROM source_files WHERE id=?1"#,
                    [id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    },
                )?;
            let client = parse_client_sql(&client_text)?;
            let state = serde_json::from_str(&state_json).unwrap_or(serde_json::Value::Null);
            let private_path = if is_private_id(&path, "evt") {
                path
            } else {
                source_storage_key(Path::new(&path))
            };
            transaction.execute(
                r#"UPDATE source_files SET path=?1, checkpoint_hash=?2,
                          head_hash=?3, adapter_state=?4, last_status=?5
                   WHERE id=?6"#,
                params![
                    private_path,
                    sanitize_hex_digest(&checkpoint_hash),
                    sanitize_hex_digest(&head_hash),
                    serde_json::to_string(&sanitize_adapter_state(client, &state))?,
                    sanitize_identifier(&last_status, 32, "unknown"),
                    id,
                ],
            )?;
        }
        last_id = *ids.last().expect("non-empty migration batch");
    }

    scrub_warning_rows_batched(transaction)?;
    scrub_scan_run_rows_batched(transaction)?;
    scrub_reconciliation_rows_batched(transaction)?;
    Ok(())
}

fn migration_id_batch(
    transaction: &Transaction<'_>,
    query: &str,
    last_id: i64,
) -> Result<Vec<i64>> {
    let mut statement = transaction.prepare(query)?;
    statement
        .query_map(params![last_id, PRIVACY_MIGRATION_BATCH_SIZE], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn scrub_warning_rows_batched(transaction: &Transaction<'_>) -> Result<()> {
    let mut last_id = 0;
    loop {
        let ids = migration_id_batch(
            transaction,
            "SELECT id FROM scan_warnings WHERE id>?1 ORDER BY id LIMIT ?2",
            last_id,
        )?;
        if ids.is_empty() {
            return Ok(());
        }
        for id in &ids {
            let (code, locator) = transaction.query_row(
                "SELECT code, locator FROM scan_warnings WHERE id=?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )?;
            transaction.execute(
                "UPDATE scan_warnings SET code=?1, message=?2, locator=?3 WHERE id=?4",
                params![
                    sanitize_warning_code(&code),
                    "warning details redacted at the storage boundary",
                    locator.as_deref().map(sanitize_source_locator),
                    id,
                ],
            )?;
        }
        last_id = *ids.last().expect("non-empty migration batch");
    }
}

fn scrub_scan_run_rows_batched(transaction: &Transaction<'_>) -> Result<()> {
    let mut last_id = 0;
    loop {
        let ids = migration_id_batch(
            transaction,
            "SELECT id FROM scan_runs WHERE id>?1 ORDER BY id LIMIT ?2",
            last_id,
        )?;
        if ids.is_empty() {
            return Ok(());
        }
        for id in &ids {
            let (mode, status) = transaction.query_row(
                "SELECT mode, status FROM scan_runs WHERE id=?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?;
            transaction.execute(
                "UPDATE scan_runs SET mode=?1, status=?2 WHERE id=?3",
                params![sanitize_scan_mode(&mode), sanitize_scan_status(&status), id],
            )?;
        }
        last_id = *ids.last().expect("non-empty migration batch");
    }
}

fn scrub_reconciliation_rows_batched(transaction: &Transaction<'_>) -> Result<()> {
    let mut last_id = 0;
    loop {
        let ids = migration_id_batch(
            transaction,
            "SELECT id FROM reconciliation_imports WHERE id>?1 ORDER BY id LIMIT ?2",
            last_id,
        )?;
        if ids.is_empty() {
            break;
        }
        for id in &ids {
            let (content_digest, source_kind, adapter, provider) = transaction.query_row(
                r#"SELECT content_digest, source_kind, adapter, provider
                   FROM reconciliation_imports WHERE id=?1"#,
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )?;
            transaction.execute(
                r#"UPDATE reconciliation_imports SET content_digest=?1,
                          source_kind=?2, adapter=?3, provider=?4 WHERE id=?5"#,
                params![
                    sanitize_content_digest(&content_digest),
                    sanitize_identifier(&source_kind, 64, "unknown_source_kind"),
                    sanitize_identifier(&adapter, 64, "unknown_adapter"),
                    provider
                        .as_deref()
                        .map(|value| sanitize_identifier(value, 64, "unknown")),
                    id,
                ],
            )?;
        }
        last_id = *ids.last().expect("non-empty migration batch");
    }

    let mut last_id = 0;
    loop {
        let ids = migration_id_batch(
            transaction,
            "SELECT id FROM reconciliation_buckets WHERE id>?1 ORDER BY id LIMIT ?2",
            last_id,
        )?;
        if ids.is_empty() {
            return Ok(());
        }
        for id in &ids {
            let (source_kind, provider, model, inference_geo, service_tier, provider_route) =
                transaction.query_row(
                    r#"SELECT source_kind, provider, model, inference_geo,
                              service_tier, provider_route
                       FROM reconciliation_buckets WHERE id=?1"#,
                    [id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                        ))
                    },
                )?;
            transaction.execute(
                r#"UPDATE reconciliation_buckets SET source_kind=?1, provider=?2,
                          model=?3, inference_geo=?4, service_tier=?5,
                          provider_route=?6 WHERE id=?7"#,
                params![
                    sanitize_identifier(&source_kind, 64, "unknown_source_kind"),
                    sanitize_identifier(&provider, 64, "unknown"),
                    model.as_deref().map(sanitize_model),
                    sanitize_optional_identifier(inference_geo.as_deref(), 64),
                    sanitize_optional_identifier(service_tier.as_deref(), 64),
                    sanitize_optional_identifier(provider_route.as_deref(), 64),
                    id,
                ],
            )?;
        }
        last_id = *ids.last().expect("non-empty migration batch");
    }
}

fn recreate_v6_identity_tables(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        r#"DROP TABLE IF EXISTS codex_event_identity_replays;
           DROP TABLE IF EXISTS codex_event_identity_aliases;
           CREATE TABLE codex_event_identity_aliases (
               source_file_id INTEGER NOT NULL REFERENCES source_files(id) ON DELETE CASCADE,
               canonical_event_key TEXT NOT NULL,
               session_scope TEXT NOT NULL,
               source_locator TEXT NOT NULL,
               usage_event_index INTEGER NOT NULL,
               PRIMARY KEY(source_file_id, canonical_event_key, session_scope)
           );
           CREATE TABLE codex_event_identity_replays (
               source_file_id INTEGER PRIMARY KEY REFERENCES source_files(id) ON DELETE CASCADE,
               globally_anchored INTEGER NOT NULL DEFAULT 0
           );"#,
    )?;
    ensure_v6_identity_indexes(transaction)
}

fn ensure_v6_identity_indexes(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        r#"CREATE INDEX IF NOT EXISTS idx_codex_event_identity_alias_lookup
               ON codex_event_identity_aliases(source_file_id, session_scope, source_locator);
           CREATE INDEX IF NOT EXISTS idx_codex_event_identity_alias_fallback
               ON codex_event_identity_aliases(source_file_id, session_scope, usage_event_index);
           CREATE INDEX IF NOT EXISTS idx_codex_event_identity_alias_global_lookup
               ON codex_event_identity_aliases(session_scope, source_locator);
           CREATE INDEX IF NOT EXISTS idx_codex_event_identity_alias_global_fallback
               ON codex_event_identity_aliases(session_scope, usage_event_index);"#,
    )?;
    Ok(())
}

fn wal_checkpoint_complete(connection: &Connection) -> Result<bool> {
    let (busy, log_frames, checkpointed_frames) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
    Ok(busy == 0 && (log_frames < 0 || checkpointed_frames >= log_frames))
}

fn rescrub_v5_private_values(transaction: &Transaction<'_>) -> Result<()> {
    let mut last_id = 0;
    loop {
        let ids = migration_id_batch(
            transaction,
            "SELECT id FROM usage_observations WHERE id>?1 ORDER BY id LIMIT ?2",
            last_id,
        )?;
        if ids.is_empty() {
            break;
        }
        for id in &ids {
            let (
                client_text,
                event_key,
                session_id,
                provider_message_id,
                raw_model,
                provider,
                dimensions_json,
                source_locator,
                parser_version,
                warnings_json,
            ) = transaction.query_row(
                r#"SELECT client, event_key, session_id, provider_message_id,
                              raw_model, provider, dimensions_json, source_locator,
                              parser_version, warnings_json
                       FROM usage_observations WHERE id=?1"#,
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )?;
            let client = parse_client_sql(&client_text)?;
            let warnings = serde_json::from_str::<Vec<String>>(&warnings_json).unwrap_or_default();
            let dimensions = serde_json::from_str::<PricingDimensions>(&dimensions_json)
                .unwrap_or_else(|_| PricingDimensions::default());
            let dimensions = sanitize_dimensions_for_storage(client, &dimensions, &warnings);
            let event_key = if is_private_id(&event_key, "evt") {
                event_key.to_ascii_lowercase()
            } else {
                crate::model::stable_id(&[client.as_str(), &event_key])
            };
            transaction.execute(
                r#"UPDATE usage_observations SET event_key=?1, session_id=?2,
                          provider_message_id=?3, raw_model=?4, provider=?5,
                          dimensions_json=?6, source_locator=?7,
                          parser_version=?8, warnings_json=?9
                   WHERE id=?10"#,
                params![
                    event_key,
                    pseudonymous_session_id(client, &session_id),
                    provider_message_id
                        .as_deref()
                        .map(|value| private_provider_message_id(client, value)),
                    sanitize_model(&raw_model),
                    sanitize_identifier(&provider, 64, "unknown"),
                    serde_json::to_string(&dimensions)?,
                    sanitize_source_locator(&source_locator),
                    sanitize_identifier(&parser_version, 96, "unknown"),
                    serde_json::to_string(&sanitize_observation_warnings(&warnings))?,
                    id,
                ],
            )?;
        }
        last_id = *ids.last().expect("non-empty migration batch");
    }

    let mut last_id = 0;
    loop {
        let ids = migration_id_batch(
            transaction,
            "SELECT id FROM source_files WHERE id>?1 ORDER BY id LIMIT ?2",
            last_id,
        )?;
        if ids.is_empty() {
            return Ok(());
        }
        for id in &ids {
            let path: String = transaction.query_row(
                "SELECT path FROM source_files WHERE id=?1",
                [id],
                |row| row.get(0),
            )?;
            let private_path = if is_private_id(&path, "evt") {
                path
            } else {
                source_storage_key(Path::new(&path))
            };
            transaction.execute(
                r#"UPDATE source_files SET path=?1, checkpoint_offset=0,
                          checkpoint_line=0, checkpoint_hash='', head_hash='',
                          adapter_state='null', last_status='partial'
                   WHERE id=?2"#,
                params![private_path, id],
            )?;
        }
        last_id = *ids.last().expect("non-empty migration batch");
    }
}

fn backfill_codex_event_identity_aliases(transaction: &Transaction<'_>) -> Result<()> {
    let mut last_source_id = 0;
    loop {
        let source_ids = migration_id_batch(
            transaction,
            r#"SELECT id FROM source_files
               WHERE client='openai_codex' AND id>?1
               ORDER BY id LIMIT ?2"#,
            last_source_id,
        )?;
        if source_ids.is_empty() {
            return Ok(());
        }
        for source_id in &source_ids {
            let mut last_observation_id = 0;
            let mut usage_event_index = 0_u64;
            loop {
                let observation_ids = {
                    let mut statement = transaction.prepare(
                        r#"SELECT id FROM usage_observations
                           WHERE source_file_id=?1 AND id>?2
                           ORDER BY id LIMIT ?3"#,
                    )?;
                    statement
                        .query_map(
                            params![source_id, last_observation_id, PRIVACY_MIGRATION_BATCH_SIZE],
                            |row| row.get::<_, i64>(0),
                        )?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                if observation_ids.is_empty() {
                    break;
                }
                for observation_id in &observation_ids {
                    let (event_key, session_id, locator) = transaction.query_row(
                        r#"SELECT event_key, session_id, source_locator
                           FROM usage_observations WHERE id=?1"#,
                        [observation_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )?;
                    usage_event_index = usage_event_index.saturating_add(1);
                    let canonical_event_key = if is_private_id(&event_key, "evt") {
                        event_key.to_ascii_lowercase()
                    } else {
                        crate::model::stable_id(&[Client::OpenaiCodex.as_str(), &event_key])
                    };
                    for session_scope in codex_migrated_session_scope_candidates(&session_id) {
                        transaction.execute(
                            r#"INSERT OR IGNORE INTO codex_event_identity_aliases(
                                   source_file_id, canonical_event_key, session_scope,
                                   source_locator, usage_event_index
                               ) VALUES (?1, ?2, ?3, ?4, ?5)"#,
                            params![
                                source_id,
                                canonical_event_key,
                                session_scope,
                                sanitize_source_locator(&locator),
                                to_i64(usage_event_index)?,
                            ],
                        )?;
                    }
                }
                last_observation_id = *observation_ids.last().expect("non-empty migration batch");
            }
        }
        last_source_id = *source_ids.last().expect("non-empty migration batch");
    }
}

fn codex_session_scope(persisted_session_id: &str) -> String {
    pseudonymous_id(
        "tlasp",
        "codex-migrated-session-scope-v2",
        &[persisted_session_id],
    )
}

fn codex_migrated_session_scope_candidates(session_id: &str) -> Vec<String> {
    let mut scopes = Vec::with_capacity(CODEX_ALIAS_SCOPE_CANDIDATE_LIMIT);
    let mut candidate = session_id.to_string();
    for _ in 0..CODEX_ALIAS_SCOPE_CANDIDATE_LIMIT {
        scopes.push(codex_session_scope(&candidate));
        candidate = pseudonymous_session_id(Client::OpenaiCodex, &candidate);
    }
    scopes.sort();
    scopes.dedup();
    scopes
}

fn migrated_codex_event_alias(
    transaction: &Transaction<'_>,
    source_id: i64,
    observation: &UsageObservation,
) -> Result<Option<String>> {
    if observation.client != Client::OpenaiCodex {
        return Ok(None);
    }
    let locator = sanitize_source_locator(&observation.source_locator);
    let persisted_session = pseudonymous_session_id(Client::OpenaiCodex, &observation.session_id);
    let session_scope = codex_session_scope(&persisted_session);

    let local_exact = codex_alias_candidates(
        transaction,
        Some(source_id),
        &session_scope,
        Some(locator.as_str()),
        None,
    )?;
    if let Some(value) = unique_codex_alias(local_exact)? {
        return Ok(Some(value));
    }
    let global_exact = codex_alias_candidates(
        transaction,
        None,
        &session_scope,
        Some(locator.as_str()),
        None,
    )?;
    if let Some(value) = unique_codex_alias(global_exact)? {
        transaction.execute(
            r#"UPDATE codex_event_identity_replays SET globally_anchored=1
               WHERE source_file_id=?1"#,
            [source_id],
        )?;
        return Ok(Some(value));
    }

    let Some(usage_event_index) = observation.usage_event_index else {
        return Ok(None);
    };
    let source_has_aliases: bool = transaction.query_row(
        r#"SELECT EXISTS(
               SELECT 1 FROM codex_event_identity_aliases
               WHERE source_file_id=?1
           )"#,
        [source_id],
        |row| row.get(0),
    )?;
    let globally_anchored: bool = transaction.query_row(
        r#"SELECT COALESCE((
               SELECT globally_anchored FROM codex_event_identity_replays
               WHERE source_file_id=?1
           ), 0)"#,
        [source_id],
        |row| row.get(0),
    )?;
    if !source_has_aliases && !globally_anchored {
        let unanchored_global = codex_alias_candidates(
            transaction,
            None,
            &session_scope,
            None,
            Some(usage_event_index),
        )?;
        if !unanchored_global.is_empty() {
            anyhow::bail!(
                "unanchored migrated Codex event identity candidate; source rebuild was rolled back"
            );
        }
        return Ok(None);
    }
    if source_has_aliases {
        let local_ordinal = codex_alias_candidates(
            transaction,
            Some(source_id),
            &session_scope,
            None,
            Some(usage_event_index),
        )?;
        if let Some(value) = unique_codex_alias(local_ordinal)? {
            return Ok(Some(value));
        }
    }
    if globally_anchored || source_has_aliases {
        let global_ordinal = codex_alias_candidates(
            transaction,
            None,
            &session_scope,
            None,
            Some(usage_event_index),
        )?;
        if let Some(value) = unique_codex_alias(global_ordinal)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn codex_alias_candidates(
    transaction: &Transaction<'_>,
    source_id: Option<i64>,
    session_scope: &str,
    locator: Option<&str>,
    usage_event_index: Option<u64>,
) -> Result<Vec<String>> {
    let values = match (source_id, locator, usage_event_index) {
        (Some(source_id), Some(locator), None) => {
            let mut statement = transaction.prepare(
                r#"SELECT canonical_event_key FROM codex_event_identity_aliases
                   WHERE source_file_id=?1 AND session_scope=?2 AND source_locator=?3
                   GROUP BY canonical_event_key ORDER BY canonical_event_key LIMIT 2"#,
            )?;
            statement
                .query_map(params![source_id, session_scope, locator], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        (None, Some(locator), None) => {
            let mut statement = transaction.prepare(
                r#"SELECT canonical_event_key FROM codex_event_identity_aliases
                   WHERE session_scope=?1 AND source_locator=?2
                   GROUP BY canonical_event_key ORDER BY canonical_event_key LIMIT 2"#,
            )?;
            statement
                .query_map(params![session_scope, locator], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        (Some(source_id), None, Some(index)) => {
            let mut statement = transaction.prepare(
                r#"SELECT canonical_event_key FROM codex_event_identity_aliases
                   WHERE source_file_id=?1 AND session_scope=?2 AND usage_event_index=?3
                   GROUP BY canonical_event_key ORDER BY canonical_event_key LIMIT 2"#,
            )?;
            statement
                .query_map(params![source_id, session_scope, to_i64(index)?], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        (None, None, Some(index)) => {
            let mut statement = transaction.prepare(
                r#"SELECT canonical_event_key FROM codex_event_identity_aliases
                   WHERE session_scope=?1 AND usage_event_index=?2
                   GROUP BY canonical_event_key ORDER BY canonical_event_key LIMIT 2"#,
            )?;
            statement
                .query_map(params![session_scope, to_i64(index)?], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        _ => anyhow::bail!("invalid migrated Codex alias lookup"),
    };
    Ok(values
        .into_iter()
        .filter(|value| is_private_id(value, "evt"))
        .collect())
}

fn unique_codex_alias(values: Vec<String>) -> Result<Option<String>> {
    match values.len() {
        0 => Ok(None),
        1 => Ok(values.into_iter().next()),
        _ => anyhow::bail!(
            "ambiguous migrated Codex event identity alias; source rebuild was rolled back"
        ),
    }
}

fn is_private_id(value: &str, prefix: &str) -> bool {
    let Some(digest) = value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_prefix('_'))
    else {
        return false;
    };
    digest.len() == 24 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn private_provider_message_id(client: Client, value: &str) -> String {
    pseudonymous_id(
        "tlmsg",
        "persisted-provider-message",
        &[client.as_str(), value],
    )
}

fn private_provider_request_id(client: Client, value: &str) -> String {
    pseudonymous_id(
        "tlreq",
        "persisted-provider-request",
        &[client.as_str(), value],
    )
}

fn migrated_session_id(client: Client, value: &str) -> String {
    if client == Client::OpenaiCodex {
        // Current Codex adapters pseudonymize session identity before it enters
        // event identity or resumable state. The storage boundary applies a
        // second, independent transform. Reproduce that pipeline for v4 rows
        // so migrated and newly scanned Codex sessions remain group-compatible.
        let adapter_private = pseudonymous_session_id(client, value);
        pseudonymous_session_id(client, &adapter_private)
    } else {
        pseudonymous_session_id(client, value)
    }
}

fn sanitize_model(value: &str) -> String {
    sanitize_identifier(value, 128, "unknown")
}

fn sanitize_optional_identifier(value: Option<&str>, maximum_length: usize) -> Option<String> {
    value.and_then(|value| {
        let sanitized = sanitize_identifier(value, maximum_length, "");
        (!sanitized.is_empty()).then_some(sanitized)
    })
}

fn sanitize_dimensions_for_storage(
    client: Client,
    dimensions: &PricingDimensions,
    warnings: &[String],
) -> PricingDimensions {
    let mut sanitized = dimensions.clone();
    if sanitized.auth_mode.is_some()
        && sanitized.auth_mode_provenance.is_none()
        && warnings.iter().any(|warning| {
            warning.contains("auth mode was inferred from the current Codex profile")
        })
    {
        sanitized.auth_mode_provenance =
            Some(crate::model::DimensionValueProvenance::CurrentProfileInferred);
    }
    if sanitized.input_subset_accounting_consistent.is_none()
        && warnings
            .iter()
            .any(|warning| warning == "input_subsets_exceed_total_input")
    {
        sanitized.input_subset_accounting_consistent = Some(false);
    }
    sanitized.provider_request_id = sanitized
        .provider_request_id
        .as_deref()
        .map(|value| private_provider_request_id(client, value));
    sanitized.auth_mode = sanitize_optional_identifier(sanitized.auth_mode.as_deref(), 64);
    sanitized.provider_route =
        sanitize_optional_identifier(sanitized.provider_route.as_deref(), 64);
    sanitized.service_tier = sanitize_optional_identifier(sanitized.service_tier.as_deref(), 64);
    sanitized.speed = sanitize_optional_identifier(sanitized.speed.as_deref(), 64);
    sanitized.inference_geo = sanitize_optional_identifier(sanitized.inference_geo.as_deref(), 64);
    sanitized
}

fn sanitize_observation_warning(value: &str) -> String {
    if value.contains("auth mode was inferred from the current Codex profile") {
        return "auth_mode_current_profile_inferred".to_string();
    }
    let safe = sanitize_identifier(value, 96, "");
    if safe.is_empty() {
        pseudonymous_id("tlwarn", "redacted-observation-warning", &[value])
    } else {
        safe
    }
}

fn sanitize_observation_warnings(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| sanitize_observation_warning(value))
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn sanitize_scan_warning(warning: &ScanWarning) -> ScanWarning {
    ScanWarning {
        code: sanitize_warning_code(&warning.code),
        message: "warning details redacted at the storage boundary".to_string(),
        locator: warning.locator.as_deref().map(sanitize_source_locator),
    }
}

fn sanitize_scan_mode(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "incremental" => "incremental".to_string(),
        "full" | "rebuild" => "full".to_string(),
        "dry_run" | "dry-run" => "dry_run".to_string(),
        #[cfg(test)]
        "test" => "test".to_string(),
        _ => "unknown".to_string(),
    }
}

fn sanitize_scan_status(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "running" => "running".to_string(),
        "ok" => "ok".to_string(),
        "partial" => "partial".to_string(),
        "abandoned" => "abandoned".to_string(),
        "failed" | "error" => "failed".to_string(),
        _ => "unknown".to_string(),
    }
}

fn sanitize_hex_digest(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        value.to_ascii_lowercase()
    } else {
        String::new()
    }
}

fn domain_digest_hex(domain: &str, value: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"token-ledger/private-digest/v1\0");
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain.as_bytes());
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
    hex::encode(hasher.finalize())
}

fn sanitize_content_digest(value: &str) -> String {
    let value = value.trim();
    let normalized = if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        value.to_ascii_lowercase()
    } else {
        value.to_string()
    };
    domain_digest_hex(
        "persisted-reconciliation-content-digest",
        normalized.as_bytes(),
    )
}

fn sanitize_adapter_state(client: Client, value: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};

    let Some(source) = value.as_object() else {
        return Value::Null;
    };
    let mut safe = Map::new();
    let session_ids_private = source
        .get("session_ids_private")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    for key in [
        "canonical_meta_seen",
        "auth_mode_inferred",
        "compressed_archive_unsupported",
    ] {
        if let Some(value) = source.get(key).and_then(Value::as_bool) {
            safe.insert(key.to_string(), Value::Bool(value));
        }
    }
    if let Some(value) = source.get("compressed_source_hash").and_then(Value::as_str) {
        let value = sanitize_hex_digest(value);
        if !value.is_empty() {
            safe.insert("compressed_source_hash".to_string(), Value::String(value));
        }
    }
    for key in ["context_window", "epoch", "usage_event_index"] {
        if let Some(value) = source.get(key).and_then(Value::as_u64) {
            safe.insert(key.to_string(), Value::from(value));
        }
    }
    let mut retained_session_id = false;
    for key in ["logical_session_id", "physical_thread_id"] {
        if let Some(value) = source.get(key).and_then(Value::as_str) {
            let value = if session_ids_private && is_private_id(value, "tlses") {
                value.to_ascii_lowercase()
            } else {
                // Untagged state is legacy/untrusted. Always transform it even
                // when its provider value mimics our private prefix and shape.
                pseudonymous_session_id(client, value)
            };
            safe.insert(key.to_string(), Value::String(value));
            retained_session_id = true;
        }
    }
    if session_ids_private || retained_session_id {
        safe.insert("session_ids_private".to_string(), Value::Bool(true));
    }
    for (key, maximum_length) in [
        ("client_version", 96),
        ("provider", 64),
        ("service_tier", 64),
        ("auth_mode", 64),
    ] {
        if let Some(value) = source.get(key).and_then(Value::as_str)
            && let Some(value) = sanitize_optional_identifier(Some(value), maximum_length)
        {
            safe.insert(key.to_string(), Value::String(value));
        }
    }
    if let Some(value) = source.get("model").and_then(Value::as_str) {
        safe.insert("model".to_string(), Value::String(sanitize_model(value)));
    }
    if let Some(previous) = source.get("previous").and_then(Value::as_object) {
        let mut counters = Map::new();
        for key in [
            "input",
            "cached_input",
            "output",
            "reasoning_output",
            "total",
            "cache_write_5m",
            "cache_write_1h",
            "cache_write_unknown",
        ] {
            if let Some(value) = previous.get(key).and_then(Value::as_u64) {
                counters.insert(key.to_string(), Value::from(value));
            }
        }
        for key in [
            "cached_input_reported",
            "reasoning_output_reported",
            "cache_write_reported",
        ] {
            if let Some(value) = previous.get(key).and_then(Value::as_bool) {
                counters.insert(key.to_string(), Value::Bool(value));
            }
        }
        safe.insert("previous".to_string(), Value::Object(counters));
    }
    if let Some(warned) = source.get("warned").and_then(Value::as_array) {
        let mut values = warned
            .iter()
            .filter_map(Value::as_str)
            .map(sanitize_warning_code)
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        safe.insert(
            "warned".to_string(),
            Value::Array(values.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(safe)
}

fn source_storage_key(path: &Path) -> String {
    crate::model::stable_id(&["source-file", &path.to_string_lossy()])
}

fn pseudonymous_source_id(source_storage_key: &str) -> String {
    pseudonymous_id("src", "provenance-source", &[source_storage_key])
}

fn sanitize_source_locator(locator: &str) -> String {
    let Some((line_start, line_digits)) = last_numeric_field(locator, "line ") else {
        return "redacted source location".to_string();
    };
    let mut sanitized = format!("line {line_digits}");
    let after_line = &locator[line_start + "line ".len() + line_digits.len()..];
    if let Some((_, byte_digits)) = first_numeric_field(after_line, "byte ") {
        sanitized.push_str(" @ byte ");
        sanitized.push_str(byte_digits);
    }
    sanitized
}

fn last_numeric_field<'a>(value: &'a str, label: &str) -> Option<(usize, &'a str)> {
    value.rmatch_indices(label).find_map(|(start, _)| {
        let digits_start = start + label.len();
        let digits_len = value[digits_start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        (digits_len > 0).then(|| (start, &value[digits_start..digits_start + digits_len]))
    })
}

fn first_numeric_field<'a>(value: &'a str, label: &str) -> Option<(usize, &'a str)> {
    value.match_indices(label).find_map(|(start, _)| {
        let digits_start = start + label.len();
        let digits_len = value[digits_start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        (digits_len > 0).then(|| (start, &value[digits_start..digits_start + digits_len]))
    })
}

fn sanitize_identifier(value: &str, maximum_length: usize, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty()
        || value.len() > maximum_length
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b';' | b'=')
        })
    {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

fn sanitize_warning_code(value: &str) -> String {
    sanitize_identifier(value, 64, "unknown_warning")
}

fn parse_datetime_sql(index: usize, value: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn parse_decimal_sql(index: usize, value: &str) -> rusqlite::Result<Decimal> {
    Decimal::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn parse_coverage(value: &str) -> CoverageStatus {
    match value {
        "complete_known" => CoverageStatus::CompleteKnown,
        "partial_known" => CoverageStatus::PartialKnown,
        _ => CoverageStatus::Unknown,
    }
}

fn parse_client_sql(value: &str) -> rusqlite::Result<Client> {
    match value {
        "claude_code" => Ok(Client::ClaudeCode),
        "openai_codex" => Ok(Client::OpenaiCodex),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn query_count(connection: &Connection, query: &str) -> Result<u64> {
    let value: i64 = connection.query_row(query, [], |row| row.get(0))?;
    from_i64(value).map_err(Into::into)
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("token count exceeds SQLite signed 64-bit range")
}

fn optional_i64(value: Option<u64>) -> Result<Option<i64>> {
    value.map(to_i64).transpose()
}

fn optional_u64_sql(value: Option<i64>) -> rusqlite::Result<Option<u64>> {
    value.map(from_i64).transpose()
}

fn from_i64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::stable_id;
    use chrono::TimeZone;
    use sha2::Digest;
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use tempfile::tempdir;

    fn emulate_legacy_schema(connection: &Connection, version: i64) -> Result<()> {
        connection.execute_batch(
            "DROP TRIGGER IF EXISTS guard_schema_version_no_downgrade;
             DROP TRIGGER IF EXISTS guard_schema_version_no_delete;
             DROP TRIGGER IF EXISTS guard_schema_version_no_replace;",
        )?;
        connection.execute(
            "UPDATE meta SET value=?1 WHERE key='schema_version'",
            [version.to_string()],
        )?;
        Ok(())
    }

    fn observation(source: &str, timestamp: i64, output: u64) -> UsageObservation {
        UsageObservation {
            event_key: "shared".into(),
            client: Client::ClaudeCode,
            session_id: source.into(),
            usage_event_index: None,
            provider_message_id: Some("msg_1".into()),
            occurred_at: Utc.timestamp_opt(timestamp, 0).unwrap(),
            raw_model: "model".into(),
            provider: "anthropic".into(),
            usage: UsageVector {
                output_tokens_total: output,
                ..Default::default()
            },
            dimensions: PricingDimensions::default(),
            quality: UsageQuality::Exact,
            coverage: CoverageStatus::Unknown,
            source_locator: "line 1".into(),
            parser_version: "test".into(),
            warnings: vec![],
        }
    }

    fn codex_observation(
        event_key: &str,
        adapter_session_id: &str,
        usage_event_index: u64,
        locator: &str,
        output: u64,
    ) -> UsageObservation {
        let mut value = observation(adapter_session_id, usage_event_index as i64, output);
        value.event_key = event_key.to_string();
        value.client = Client::OpenaiCodex;
        value.session_id = adapter_session_id.to_string();
        value.usage_event_index = Some(usage_event_index);
        value.provider_message_id = None;
        value.provider = "openai".to_string();
        value.raw_model = "gpt-5.4".to_string();
        value.source_locator = locator.to_string();
        value
    }

    fn assert_database_family_excludes(database: &Path, markers: &[&str]) -> Result<()> {
        let directory = database.parent().context("database has no parent")?;
        let base = database
            .file_name()
            .and_then(|value| value.to_str())
            .context("database filename is not UTF-8")?;
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || !entry.file_name().to_string_lossy().starts_with(base)
            {
                continue;
            }
            let bytes = std::fs::read(entry.path())?;
            for marker in markers {
                assert!(
                    !bytes
                        .windows(marker.len())
                        .any(|window| window == marker.as_bytes()),
                    "private marker {marker:?} remained in {}",
                    entry.path().display()
                );
            }
        }
        Ok(())
    }

    #[test]
    fn scan_lease_rejects_a_concurrent_writer_then_allows_the_next_run() -> Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("test.sqlite");
        let mut first = Ledger::open(&database)?;
        let mut second = Ledger::open(&database)?;

        let first_run = first.start_scan("incremental")?;
        let error = second
            .start_scan("incremental")
            .expect_err("a fresh scan lease must reject another writer");
        assert!(error.to_string().contains("already in progress"));

        first.finish_scan(first_run, 0, 0, 0, "ok")?;
        let second_run = second.start_scan("incremental")?;
        second.finish_scan(second_run, 0, 0, 0, "ok")?;
        Ok(())
    }

    #[test]
    fn completed_scan_history_and_warnings_are_bounded() -> Result<()> {
        let dir = tempdir()?;
        let mut ledger = Ledger::open(&dir.path().join("bounded-history.sqlite"))?;
        let total = COMPLETED_SCAN_HISTORY_LIMIT + 3;
        for _ in 0..total {
            let run = ledger.start_scan("test")?;
            ledger.record_scan_warning(
                run,
                None,
                &ScanWarning::new("bounded_history", "private diagnostic text"),
            )?;
            ledger.finish_scan(run, 0, 0, 1, "partial")?;
        }

        let runs: i64 =
            ledger
                .connection
                .query_row("SELECT COUNT(*) FROM scan_runs", [], |row| row.get(0))?;
        let warnings: i64 =
            ledger
                .connection
                .query_row("SELECT COUNT(*) FROM scan_warnings", [], |row| row.get(0))?;
        assert_eq!(runs, COMPLETED_SCAN_HISTORY_LIMIT);
        assert_eq!(warnings, COMPLETED_SCAN_HISTORY_LIMIT);
        assert_eq!(ledger.warning_code_counts()?[0].count, 1);

        let clean_run = ledger.start_scan("test")?;
        ledger.finish_scan(clean_run, 0, 0, 0, "ok")?;
        assert!(ledger.warning_code_counts()?.is_empty());
        Ok(())
    }

    #[test]
    fn source_status_is_partial_until_checkpoint_reaches_file_size() -> Result<()> {
        let directory = tempdir()?;
        let mut ledger = Ledger::open(&directory.path().join("partial.sqlite"))?;
        let run = ledger.start_scan("test")?;
        let source_id = ledger.ensure_source(
            Client::ClaudeCode,
            &directory.path().join("session.jsonl"),
            false,
        )?;
        let state = serde_json::Value::Null;
        for (checkpoint_offset, expected) in [(5, "partial"), (10, "ok")] {
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 10,
                modified_ns: 1,
                checkpoint_offset,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[],
                warnings: &[],
                scan_run_id: run,
            })?;
            let status: String = ledger.connection.query_row(
                "SELECT last_status FROM source_files WHERE id=?1",
                [source_id],
                |row| row.get(0),
            )?;
            assert_eq!(status, expected);
        }
        ledger.finish_scan(run, 1, 0, 0, "ok")?;
        Ok(())
    }

    #[test]
    fn simultaneous_scan_contenders_cannot_both_acquire_the_lease() -> Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("test.sqlite");
        drop(Ledger::open(&database)?);

        let start = Arc::new(Barrier::new(3));
        let release = Arc::new(Barrier::new(3));
        let (sender, receiver) = mpsc::channel();
        let mut handles = Vec::new();
        for _ in 0..2 {
            let database = database.clone();
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            let sender = sender.clone();
            handles.push(thread::spawn(move || -> Result<()> {
                let mut ledger = Ledger::open(&database)?;
                start.wait();
                let acquired = ledger.start_scan("incremental");
                sender.send((
                    acquired.is_ok(),
                    acquired
                        .as_ref()
                        .err()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                ))?;
                release.wait();
                if let Ok(run) = acquired {
                    ledger.finish_scan(run, 0, 0, 0, "ok")?;
                }
                Ok(())
            }));
        }
        drop(sender);

        start.wait();
        let results = [receiver.recv()?, receiver.recv()?];
        assert_eq!(results.iter().filter(|(ok, _)| *ok).count(), 1);
        assert_eq!(results.iter().filter(|(ok, _)| !*ok).count(), 1);
        assert!(
            results
                .iter()
                .find(|(ok, _)| !*ok)
                .is_some_and(|(_, error)| error.contains("already in progress"))
        );

        release.wait();
        for handle in handles {
            handle.join().expect("scan contender panicked")?;
        }
        Ok(())
    }

    #[test]
    fn stale_scan_lease_is_marked_abandoned_before_recovery() -> Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("test.sqlite");
        let mut crashed = Ledger::open(&database)?;
        let crashed_run = crashed.start_scan("full")?;
        let stale = (Utc::now() - TimeDelta::hours(2)).to_rfc3339();
        crashed.connection.execute(
            "UPDATE scan_runs SET heartbeat_at=?1 WHERE id=?2",
            params![stale, crashed_run],
        )?;

        let mut recovered = Ledger::open(&database)?;
        let recovered_run = recovered.start_scan("incremental")?;
        let abandoned: (String, i64, Option<String>) = recovered.connection.query_row(
            "SELECT status, provisional, completed_at FROM scan_runs WHERE id=?1",
            [crashed_run],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(abandoned.0, "abandoned");
        assert_eq!(abandoned.1, 1);
        assert!(abandoned.2.is_some());
        assert!(crashed.heartbeat_scan(crashed_run).is_err());

        recovered.finish_scan(recovered_run, 0, 0, 0, "ok")?;
        let latest = recovered.latest_scan()?.expect("recovered scan snapshot");
        assert_eq!(latest.status, "ok");
        assert!(!latest.provisional);
        assert!(latest.as_of.is_some());
        Ok(())
    }

    #[test]
    fn schema_v2_scan_rows_migrate_without_losing_history() -> Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("v2.sqlite");
        {
            let connection = Connection::open(&database)?;
            connection.execute_batch(
                r#"
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO meta(key, value) VALUES ('schema_version', '2');
                CREATE TABLE scan_runs (
                    id INTEGER PRIMARY KEY,
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    mode TEXT NOT NULL,
                    source_count INTEGER NOT NULL DEFAULT 0,
                    observation_count INTEGER NOT NULL DEFAULT 0,
                    warning_count INTEGER NOT NULL DEFAULT 0,
                    status TEXT NOT NULL DEFAULT 'running'
                );
                INSERT INTO scan_runs(started_at, completed_at, mode, status)
                VALUES ('2026-07-10T10:00:00Z', '2026-07-10T10:01:00Z', 'full', 'ok');
                "#,
            )?;
        }

        let ledger = Ledger::open(&database)?;
        let snapshot = ledger.latest_scan()?.expect("migrated scan row");
        assert_eq!(snapshot.status, "ok");
        assert_eq!(snapshot.as_of, snapshot.completed_at);
        assert_eq!(snapshot.active_or_volatile_source_count, 0);
        assert!(!snapshot.provisional);
        let version: i64 = ledger.connection.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(version, SCHEMA_VERSION);
        Ok(())
    }

    #[test]
    fn schema_v3_migrates_to_separate_reconciliation_tables() -> Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("v3.sqlite");
        {
            let connection = Connection::open(&database)?;
            connection.execute_batch(
                r#"
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO meta(key, value) VALUES ('schema_version', '3');
                "#,
            )?;
        }
        let ledger = Ledger::open(&database)?;
        let version: i64 = ledger.connection.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(version, SCHEMA_VERSION);
        for table in ["reconciliation_imports", "reconciliation_buckets"] {
            let present: i64 = ledger.connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )?;
            assert_eq!(present, 1, "missing migrated table {table}");
        }
        assert!(ledger.reconciliation_imports()?.is_empty());
        assert!(ledger.reconciliation_buckets()?.is_empty());
        Ok(())
    }

    #[test]
    fn provider_identifiers_are_not_persisted_from_native_exports() -> Result<()> {
        use crate::reconcile::{ImportFormat, parse_import};

        let dir = tempdir()?;
        let database = dir.path().join("privacy.sqlite");
        let mut ledger = Ledger::open(&database)?;
        let openai_bytes =
            include_bytes!("../tests/fixtures/openai_organization_usage.json").as_slice();
        let anthropic_bytes =
            include_bytes!("../tests/fixtures/anthropic_admin_usage.json").as_slice();
        let raw_openai_digest = hex::encode(sha2::Sha256::digest(openai_bytes));
        let raw_anthropic_digest = hex::encode(sha2::Sha256::digest(anthropic_bytes));
        for (bytes, format) in [
            (openai_bytes, ImportFormat::Openai),
            (anthropic_bytes, ImportFormat::Anthropic),
        ] {
            let parsed = parse_import(bytes, format)?;
            ledger.store_reconciliation_import(&parsed)?;
        }
        ledger
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(ledger);

        let bytes = std::fs::read(&database)?;
        for canary in [
            "project-private-canary",
            "user-private-canary",
            "key-private-canary",
            "workspace-private-canary",
            raw_openai_digest.as_str(),
            raw_anthropic_digest.as_str(),
        ] {
            assert!(
                !bytes
                    .windows(canary.len())
                    .any(|window| window == canary.as_bytes()),
                "provider identifier was retained in the database"
            );
        }
        Ok(())
    }

    #[test]
    fn storage_boundary_pseudonymizes_identifiers_and_redacts_free_text() -> Result<()> {
        const EVENT: &str = "event-private-canary-a812";
        const SESSION: &str = "session-private-canary-b913";
        const MESSAGE: &str = "message-private-canary-c014";
        const REQUEST: &str = "request-private-canary-d125";
        const THREAD: &str = "thread-private-canary-e236";
        const PATH_MARKER: &str = "path-private-canary-f347";
        const TRANSCRIPT: &str = "transcript-private-canary-g458";

        let directory = tempdir()?;
        let database = directory.path().join("privacy.sqlite");
        let source_path = directory.path().join(PATH_MARKER).join("session.jsonl");
        let mut ledger = Ledger::open(&database)?;
        let run = ledger.start_scan("test")?;
        let source_id = ledger.ensure_source(Client::ClaudeCode, &source_path, false)?;
        let state = serde_json::json!({
            "canonical_meta_seen": true,
            "logical_session_id": SESSION,
            "physical_thread_id": THREAD,
            "client_version": "0.1.0",
            "model": "claude-sonnet-4-6",
            "provider": "anthropic",
            "unknown_transcript_field": TRANSCRIPT,
            "previous": {"input": 10, "output": 2, "cached_input_reported": true}
        });
        let mut value = observation(SESSION, 10, 2);
        value.event_key = EVENT.to_string();
        value.session_id = SESSION.to_string();
        value.provider_message_id = Some(MESSAGE.to_string());
        value.dimensions.provider_request_id = Some(REQUEST.to_string());
        value.source_locator = format!("C:\\private\\{PATH_MARKER}:line 7 @ byte 19");
        value.parser_version = format!("C:\\private\\{TRANSCRIPT}");
        value.warnings = vec![format!("raw transcript body {TRANSCRIPT}")];
        let warning = ScanWarning::new(
            "parse_issue",
            format!("could not parse transcript {TRANSCRIPT}"),
        )
        .at(format!("C:\\private\\{PATH_MARKER}:line 8 @ byte 20"));
        ledger.apply_source_update(SourceUpdate {
            source_id,
            reset_observations: false,
            file_size: 1,
            modified_ns: 1,
            checkpoint_offset: 1,
            checkpoint_line: 1,
            checkpoint_hash: TRANSCRIPT,
            head_hash: PATH_MARKER,
            adapter_state: &state,
            observations: &[value],
            warnings: &[warning],
            scan_run_id: run,
        })?;
        ledger.finish_scan(run, 1, 1, 1, "ok")?;

        let stored: (
            String,
            String,
            Option<String>,
            String,
            String,
            String,
            String,
        ) = ledger.connection.query_row(
            r#"SELECT event_key, session_id, provider_message_id,
                          dimensions_json, parser_version, warnings_json,
                          source_locator
                   FROM usage_observations"#,
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )?;
        assert_eq!(stored.0, stable_id(&[Client::ClaudeCode.as_str(), EVENT]));
        assert!(stored.1.starts_with("tlses_"));
        assert!(
            stored
                .2
                .as_deref()
                .is_some_and(|value| value.starts_with("tlmsg_"))
        );
        assert!(stored.3.contains("tlreq_"));
        assert_eq!(stored.4, "unknown");
        assert!(stored.5.contains("tlwarn_"));
        assert_eq!(stored.6, "line 7 @ byte 19");

        let checkpoint = ledger.source_checkpoint(&source_path)?.expect("checkpoint");
        let checkpoint_json = serde_json::to_string(&checkpoint.adapter_state)?;
        assert!(checkpoint_json.contains("tlses_"));
        assert!(!checkpoint_json.contains(SESSION));
        assert!(!checkpoint_json.contains(THREAD));
        assert!(!checkpoint_json.contains(TRANSCRIPT));
        assert!(checkpoint.checkpoint_hash.is_empty());
        assert!(checkpoint.head_hash.is_empty());

        let event = ledger.canonical_events(None, None)?.remove(0);
        let exported = serde_json::to_string(&event)?;
        for marker in [
            EVENT,
            SESSION,
            MESSAGE,
            REQUEST,
            THREAD,
            PATH_MARKER,
            TRANSCRIPT,
        ] {
            assert!(!exported.contains(marker), "event export leaked {marker}");
        }
        let stored_warning: (String, String, String) = ledger.connection.query_row(
            "SELECT code, message, locator FROM scan_warnings",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(stored_warning.0, "parse_issue");
        assert_eq!(
            stored_warning.1,
            "warning details redacted at the storage boundary"
        );
        assert_eq!(stored_warning.2, "line 8 @ byte 20");

        ledger
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(ledger);
        assert_database_family_excludes(
            &database,
            &[
                EVENT,
                SESSION,
                MESSAGE,
                REQUEST,
                THREAD,
                PATH_MARKER,
                TRANSCRIPT,
            ],
        )?;
        Ok(())
    }

    #[test]
    fn storage_boundary_transforms_untrusted_pseudonym_lookalikes() -> Result<()> {
        const SESSION: &str = "tlses_0123456789abcdef01234567";
        const THREAD: &str = "tlses_89abcdef0123456701234567";
        const MESSAGE: &str = "tlmsg_0123456789abcdef01234567";
        const REQUEST: &str = "tlreq_0123456789abcdef01234567";

        let directory = tempdir()?;
        let database = directory.path().join("lookalike-privacy.sqlite");
        let source_path = directory.path().join("lookalike.jsonl");
        let mut ledger = Ledger::open(&database)?;
        let run = ledger.start_scan("test")?;
        let source_id = ledger.ensure_source(Client::ClaudeCode, &source_path, false)?;
        let state = serde_json::json!({
            "logical_session_id": SESSION,
            "physical_thread_id": THREAD
        });
        let mut value = observation(SESSION, 10, 2);
        value.session_id = SESSION.to_string();
        value.provider_message_id = Some(MESSAGE.to_string());
        value.dimensions.provider_request_id = Some(REQUEST.to_string());
        ledger.apply_source_update(SourceUpdate {
            source_id,
            reset_observations: false,
            file_size: 1,
            modified_ns: 1,
            checkpoint_offset: 1,
            checkpoint_line: 1,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state,
            observations: &[value],
            warnings: &[],
            scan_run_id: run,
        })?;
        ledger.finish_scan(run, 1, 1, 0, "ok")?;

        let event = ledger.canonical_events(None, None)?.remove(0);
        assert_eq!(
            event.session_id,
            pseudonymous_session_id(Client::ClaudeCode, SESSION)
        );
        assert_eq!(
            event.provider_message_id.as_deref(),
            Some(private_provider_message_id(Client::ClaudeCode, MESSAGE).as_str())
        );
        assert_eq!(
            event.dimensions.provider_request_id.as_deref(),
            Some(private_provider_request_id(Client::ClaudeCode, REQUEST).as_str())
        );
        let checkpoint = ledger.source_checkpoint(&source_path)?.expect("checkpoint");
        assert_eq!(
            checkpoint
                .adapter_state
                .get("session_ids_private")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            checkpoint
                .adapter_state
                .get("logical_session_id")
                .and_then(serde_json::Value::as_str),
            Some(pseudonymous_session_id(Client::ClaudeCode, SESSION).as_str())
        );
        assert_eq!(
            checkpoint
                .adapter_state
                .get("physical_thread_id")
                .and_then(serde_json::Value::as_str),
            Some(pseudonymous_session_id(Client::ClaudeCode, THREAD).as_str())
        );
        let exported = serde_json::to_string(&event)?;
        for lookalike in [SESSION, THREAD, MESSAGE, REQUEST] {
            assert!(!exported.contains(lookalike));
        }

        ledger
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(ledger);
        assert_database_family_excludes(&database, &[SESSION, THREAD, MESSAGE, REQUEST])?;
        Ok(())
    }

    #[test]
    fn schema_v4_migration_physically_scrubs_legacy_private_values() -> Result<()> {
        const EVENT: &str = "legacy-event-private-canary-a19";
        const SESSION: &str = "tlses_abcdef0123456789abcdef01";
        const MESSAGE: &str = "tlmsg_abcdef0123456789abcdef01";
        const REQUEST: &str = "tlreq_abcdef0123456789abcdef01";
        const THREAD: &str = "tlses_123456789abcdef012345678";
        const PATH_MARKER: &str = "legacy-path-private-canary-f64";
        const TRANSCRIPT: &str = "legacy-transcript-private-canary-g75";
        const ACCOUNT: &str = "legacy-account-private-canary-h86";

        let directory = tempdir()?;
        let database = directory.path().join("legacy.sqlite");
        let source_path = directory.path().join(PATH_MARKER).join("session.jsonl");
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(Client::OpenaiCodex, &source_path, false)?;
            let state = serde_json::Value::Null;
            let mut value = observation("safe", 10, 2);
            value.client = Client::OpenaiCodex;
            value.provider = "openai".to_string();
            let warning = ScanWarning::new("safe_warning", "safe warning");
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[value],
                warnings: &[warning],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, 1, 1, "ok")?;

            let dimensions = serde_json::json!({"provider_request_id": REQUEST});
            let adapter_state = serde_json::json!({
                "canonical_meta_seen": true,
                "logical_session_id": SESSION,
                "physical_thread_id": THREAD,
                "unknown_transcript_field": TRANSCRIPT
            });
            emulate_legacy_schema(&ledger.connection, 4)?;
            ledger.connection.execute(
                r#"UPDATE usage_observations SET event_key=?1, session_id=?2,
                          provider_message_id=?3, dimensions_json=?4,
                          source_locator=?5, parser_version=?6, warnings_json=?7"#,
                params![
                    EVENT,
                    SESSION,
                    MESSAGE,
                    serde_json::to_string(&dimensions)?,
                    format!("C:\\private\\{PATH_MARKER}:line 4 @ byte 12"),
                    format!("C:\\private\\{TRANSCRIPT}"),
                    serde_json::to_string(&vec![format!("raw transcript {TRANSCRIPT}")])?,
                ],
            )?;
            ledger.connection.execute(
                r#"UPDATE source_files SET path=?1, checkpoint_hash=?2,
                          head_hash=?3, adapter_state=?4"#,
                params![
                    source_path.to_string_lossy(),
                    TRANSCRIPT,
                    PATH_MARKER,
                    serde_json::to_string(&adapter_state)?,
                ],
            )?;
            ledger.connection.execute(
                "UPDATE scan_warnings SET message=?1, locator=?2",
                params![
                    format!("raw transcript {TRANSCRIPT}"),
                    format!("C:\\private\\{PATH_MARKER}:line 5 @ byte 13"),
                ],
            )?;
            ledger.connection.execute(
                "INSERT INTO reconciliation_imports(content_digest, source_kind, adapter, provider, imported_at, byte_count, bucket_count) VALUES (?1, 'test_usage', 'test_adapter', 'openai', ?2, 1, 0)",
                params![ACCOUNT, Utc::now().to_rfc3339()],
            )?;
        }

        let ledger = Ledger::open(&database)?;
        let version: i64 = ledger.connection.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(version, SCHEMA_VERSION);
        let event = ledger.canonical_events(None, None)?.remove(0);
        assert_eq!(
            event.event_id,
            stable_id(&[Client::OpenaiCodex.as_str(), EVENT])
        );
        assert_eq!(event.event_key, event.event_id);
        let adapter_private_session = pseudonymous_session_id(Client::OpenaiCodex, SESSION);
        let v4_stored_session =
            pseudonymous_session_id(Client::OpenaiCodex, &adapter_private_session);
        assert_eq!(
            event.session_id,
            pseudonymous_session_id(Client::OpenaiCodex, &v4_stored_session)
        );
        let v4_message = private_provider_message_id(Client::OpenaiCodex, MESSAGE);
        assert_eq!(
            event.provider_message_id.as_deref(),
            Some(private_provider_message_id(Client::OpenaiCodex, &v4_message).as_str())
        );
        let v4_request = private_provider_request_id(Client::OpenaiCodex, REQUEST);
        assert_eq!(
            event.dimensions.provider_request_id.as_deref(),
            Some(private_provider_request_id(Client::OpenaiCodex, &v4_request).as_str())
        );
        let checkpoint = ledger.source_checkpoint(&source_path)?.expect("checkpoint");
        assert_eq!(checkpoint.checkpoint_offset, 0);
        assert!(checkpoint.adapter_state.is_null());
        let import_digest: String = ledger.connection.query_row(
            "SELECT content_digest FROM reconciliation_imports",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(import_digest.len(), 64);
        assert_ne!(import_digest, ACCOUNT);

        let exported = serde_json::to_string(&event)?;
        for marker in [
            EVENT,
            SESSION,
            MESSAGE,
            REQUEST,
            THREAD,
            PATH_MARKER,
            TRANSCRIPT,
            ACCOUNT,
        ] {
            assert!(
                !exported.contains(marker),
                "migrated export leaked {marker}"
            );
        }
        ledger
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(ledger);
        assert_database_family_excludes(
            &database,
            &[
                EVENT,
                SESSION,
                MESSAGE,
                REQUEST,
                THREAD,
                PATH_MARKER,
                TRANSCRIPT,
                ACCOUNT,
            ],
        )?;
        Ok(())
    }

    #[test]
    fn canonicalization_keeps_earliest_timestamp_and_max_usage() -> Result<()> {
        let dir = tempdir()?;
        let mut ledger = Ledger::open(&dir.path().join("test.sqlite"))?;
        let run = ledger.start_scan("test")?;
        for (name, timestamp, output) in [("a", 20, 5), ("b", 10, 7)] {
            let path = dir.path().join(format!("{name}.jsonl"));
            let source_id = ledger.ensure_source(Client::ClaudeCode, &path, false)?;
            let state = serde_json::json!({});
            let obs = vec![observation(name, timestamp, output)];
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 0,
                modified_ns: 0,
                checkpoint_offset: 0,
                checkpoint_line: 0,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &obs,
                warnings: &[],
                scan_run_id: run,
            })?;
        }
        let events = ledger.canonical_events(None, None)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].occurred_at.timestamp(), 10);
        assert_eq!(events[0].usage.output_tokens_total, 7);
        assert_eq!(events[0].source_count, 2);
        Ok(())
    }

    #[test]
    fn schema_v5_lookalike_state_is_scrubbed_and_rebuild_reuses_legacy_identity() -> Result<()> {
        const LOOKALIKE: &str = "tlses_fedcba9876543210fedcba98";

        let directory = tempdir()?;
        let database = directory.path().join("schema-v5-lookalike.sqlite");
        let source_path = directory.path().join("rollout.jsonl");
        let occurred_at = Utc.timestamp_opt(10, 0).unwrap();
        let boundary = "i=0;ci=0;o=2;ro=0;t=2;w5=0;w1=0;wu=0";
        let legacy_adapter_event = stable_id(&["codex-counter-boundary", LOOKALIKE, "0", boundary]);
        let adapter_private = pseudonymous_session_id(Client::OpenaiCodex, LOOKALIKE);
        let rebuilt_adapter_event =
            stable_id(&["codex-counter-boundary", &adapter_private, "0", boundary]);
        let legacy_canonical = stable_id(&[Client::OpenaiCodex.as_str(), &legacy_adapter_event]);
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(Client::OpenaiCodex, &source_path, false)?;
            let state = serde_json::json!({
                "session_ids_private": true,
                "logical_session_id": adapter_private,
                "physical_thread_id": adapter_private
            });
            let mut value = observation(LOOKALIKE, 10, 2);
            value.client = Client::OpenaiCodex;
            value.event_key = legacy_adapter_event;
            value.session_id = LOOKALIKE.to_string();
            value.provider = "openai".to_string();
            value.raw_model = "gpt-5.4".to_string();
            value.source_locator = "line 4 @ byte 12".to_string();
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[value],
                warnings: &[],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, 1, 0, "ok")?;

            // Recreate the prefix-trusting schema-v5 storage bug: both the
            // observation and parser state retained the provider lookalike.
            emulate_legacy_schema(&ledger.connection, 5)?;
            let legacy_state = serde_json::json!({
                "canonical_meta_seen": true,
                "logical_session_id": LOOKALIKE,
                "physical_thread_id": LOOKALIKE,
                "epoch": 0
            });
            ledger
                .connection
                .execute("UPDATE usage_observations SET session_id=?1", [LOOKALIKE])?;
            ledger.connection.execute(
                "UPDATE source_files SET adapter_state=?1 WHERE id=?2",
                params![legacy_state.to_string(), source_id],
            )?;
        }

        let mut ledger = Ledger::open(&database)?;
        let version: i64 = ledger.connection.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(version, SCHEMA_VERSION);
        let checkpoint = ledger.source_checkpoint(&source_path)?.expect("checkpoint");
        assert_eq!(checkpoint.checkpoint_offset, 0);
        assert!(checkpoint.adapter_state.is_null());
        let aliases: i64 = ledger.connection.query_row(
            "SELECT COUNT(*) FROM codex_event_identity_aliases",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            aliases, CODEX_ALIAS_SCOPE_CANDIDATE_LIMIT as i64,
            "legacy observations retain only the bounded scope candidates"
        );

        let run = ledger.start_scan("test")?;
        let mut rebuilt = observation("safe", 10, 2);
        rebuilt.client = Client::OpenaiCodex;
        rebuilt.event_key = rebuilt_adapter_event;
        rebuilt.session_id = adapter_private.clone();
        rebuilt.provider = "openai".to_string();
        rebuilt.raw_model = "gpt-5.4".to_string();
        rebuilt.occurred_at = occurred_at;
        rebuilt.source_locator = "line 4 @ byte 12".to_string();
        let safe_state = serde_json::json!({
            "session_ids_private": true,
            "logical_session_id": adapter_private,
            "physical_thread_id": adapter_private
        });
        ledger.apply_source_update(SourceUpdate {
            source_id: checkpoint.id,
            reset_observations: true,
            file_size: 1,
            modified_ns: 2,
            checkpoint_offset: 1,
            checkpoint_line: 1,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &safe_state,
            observations: &[rebuilt],
            warnings: &[],
            scan_run_id: run,
        })?;
        ledger.finish_scan(run, 1, 1, 0, "ok")?;
        let event = ledger.canonical_events(None, None)?.remove(0);
        assert_eq!(event.event_id, legacy_canonical);
        assert_ne!(event.session_id, LOOKALIKE);
        ledger
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(ledger);
        assert_database_family_excludes(&database, &[LOOKALIKE])?;
        Ok(())
    }

    #[test]
    fn schema_v5_reapplies_every_identifier_boundary_and_physically_scrubs_bytes() -> Result<()> {
        const SESSION: &str = "tlses_aaaaaaaaaaaaaaaaaaaaaaaa";
        const THREAD: &str = "tlses_dddddddddddddddddddddddd";
        const MESSAGE: &str = "tlmsg_bbbbbbbbbbbbbbbbbbbbbbbb";
        const REQUEST: &str = "tlreq_cccccccccccccccccccccccc";

        let directory = tempdir()?;
        let database = directory.path().join("schema-v5-all-identifiers.sqlite");
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            for (ordinal, client) in [Client::ClaudeCode, Client::OpenaiCodex]
                .into_iter()
                .enumerate()
            {
                let source_path = directory.path().join(format!("source-{ordinal}.jsonl"));
                let source_id = ledger.ensure_source(client, &source_path, false)?;
                let mut value = observation("initial-session", ordinal as i64 + 1, 2);
                value.client = client;
                value.event_key = format!("event-{ordinal}");
                value.usage_event_index =
                    (client == Client::OpenaiCodex).then_some(ordinal as u64 + 1);
                value.dimensions.provider_request_id = Some("initial-request".to_string());
                let state = serde_json::json!({
                    "logical_session_id": "initial-session",
                    "physical_thread_id": "initial-thread"
                });
                ledger.apply_source_update(SourceUpdate {
                    source_id,
                    reset_observations: false,
                    file_size: 1,
                    modified_ns: 1,
                    checkpoint_offset: 1,
                    checkpoint_line: 1,
                    checkpoint_hash: "",
                    head_hash: "",
                    adapter_state: &state,
                    observations: &[value],
                    warnings: &[],
                    scan_run_id: run,
                })?;
                let crafted_state = serde_json::json!({
                    "session_ids_private": true,
                    "logical_session_id": SESSION,
                    "physical_thread_id": THREAD,
                    "usage_event_index": 1
                });
                emulate_legacy_schema(&ledger.connection, 5)?;
                ledger.connection.execute(
                    r#"UPDATE source_files SET adapter_state=?1 WHERE id=?2"#,
                    params![crafted_state.to_string(), source_id],
                )?;
                ledger.connection.execute(
                    r#"UPDATE usage_observations
                       SET session_id=?1, provider_message_id=?2,
                           dimensions_json=?3
                       WHERE source_file_id=?4"#,
                    params![
                        SESSION,
                        MESSAGE,
                        serde_json::json!({"provider_request_id": REQUEST}).to_string(),
                        source_id,
                    ],
                )?;
            }
            ledger.finish_scan(run, 2, 2, 0, "ok")?;
        }

        let ledger = Ledger::open(&database)?;
        let stored = {
            let mut statement = ledger.connection.prepare(
                r#"SELECT client, session_id, provider_message_id, dimensions_json
                   FROM usage_observations ORDER BY client"#,
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        assert_eq!(stored.len(), 2);
        for (client_text, session, message, dimensions) in stored {
            let client = parse_client_sql(&client_text)?;
            let dimensions: PricingDimensions = serde_json::from_str(&dimensions)?;
            assert_eq!(session, pseudonymous_session_id(client, SESSION));
            assert_eq!(message, private_provider_message_id(client, MESSAGE));
            assert_eq!(
                dimensions.provider_request_id.as_deref(),
                Some(private_provider_request_id(client, REQUEST).as_str())
            );
        }
        assert!(
            ledger
                .source_rows()?
                .iter()
                .all(|source| source.checkpoint_offset == 0 && source.adapter_state.is_null())
        );
        ledger
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(ledger);
        assert_database_family_excludes(&database, &[SESSION, THREAD, MESSAGE, REQUEST])?;
        Ok(())
    }

    #[test]
    fn privacy_migration_serializes_live_legacy_writer_and_rejects_later_legacy_writes()
    -> Result<()> {
        const RAW_BEFORE: &str = "legacy-session-committed-before-migration";
        const RAW_AFTER: &str = "legacy-session-attempted-after-migration";

        let directory = tempdir()?;
        let database = directory.path().join("mixed-version-race.sqlite");
        let source_path = directory.path().join("source.jsonl");
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(Client::ClaudeCode, &source_path, false)?;
            let value = observation("initial", 1, 2);
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &serde_json::Value::Null,
                observations: &[value],
                warnings: &[],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, 1, 0, "ok")?;
            emulate_legacy_schema(&ledger.connection, 5)?;
        }

        // This connection represents v0.3: it opened the old schema before
        // v0.4 began and does not know about the v0.4 scan lease or generation
        // columns. Its in-flight write must commit before migration can begin.
        let legacy = Connection::open(&database)?;
        legacy.busy_timeout(std::time::Duration::from_secs(5))?;
        legacy.execute_batch("BEGIN IMMEDIATE")?;
        legacy.execute("UPDATE usage_observations SET session_id=?1", [RAW_BEFORE])?;

        let migration_database = database.clone();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (done_sender, done_receiver) = std::sync::mpsc::channel();
        let migration = std::thread::spawn(move || -> Result<()> {
            started_sender.send(()).expect("migration start signal");
            drop(Ledger::open(&migration_database)?);
            done_sender.send(()).expect("migration completion signal");
            Ok(())
        });
        started_receiver.recv()?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            matches!(
                done_receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "migration must not pass a live legacy writer transaction"
        );
        legacy.execute_batch("COMMIT")?;
        migration.join().expect("migration thread")?;
        done_receiver.recv()?;

        let stored: String = legacy.query_row(
            "SELECT session_id FROM usage_observations LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            stored,
            pseudonymous_session_id(Client::ClaudeCode, RAW_BEFORE),
            "the legacy write committed before the exclusive migration must be scrubbed"
        );

        let error = legacy
            .execute("UPDATE usage_observations SET session_id=?1", [RAW_AFTER])
            .expect_err("an already-open legacy connection must be rejected after migration");
        assert!(error.to_string().contains("privacy generation guard"));
        let error = legacy
            .execute(
                "UPDATE source_files SET adapter_state=?1",
                [r#"{"logical_session_id":"raw-after-migration"}"#],
            )
            .expect_err("legacy parser-state writes must also be rejected after migration");
        assert!(error.to_string().contains("privacy generation guard"));
        let stored: String = legacy.query_row(
            "SELECT session_id FROM usage_observations LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        assert!(!stored.contains(RAW_AFTER));
        Ok(())
    }

    #[test]
    fn pre_barrier_v6_cache_is_invalidated_instead_of_blessing_contamination() -> Result<()> {
        const RAW_SESSION: &str = "raw-session-reintroduced-after-v040-migration";

        let directory = tempdir()?;
        let database = directory.path().join("contaminated-v040.sqlite");
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(
                Client::ClaudeCode,
                &directory.path().join("source.jsonl"),
                false,
            )?;
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &serde_json::Value::Null,
                observations: &[observation("safe", 1, 2)],
                warnings: &[],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, 1, 0, "ok")?;

            emulate_legacy_schema(&ledger.connection, 6)?;
            let transaction = ledger.connection.transaction()?;
            drop_privacy_write_guard_triggers(&transaction)?;
            transaction.execute(
                "UPDATE usage_observations SET session_id=?1, privacy_write_generation=1",
                [RAW_SESSION],
            )?;
            transaction.execute(
                r#"INSERT INTO meta(key, value) VALUES (?1, ?2)
                   ON CONFLICT(key) DO UPDATE SET value=excluded.value"#,
                params![V6_PRIVACY_STATE_KEY, V6_PRIVACY_COMPLETE],
            )?;
            transaction.execute("DELETE FROM meta WHERE key=?1", [V7_PRIVACY_BARRIER_KEY])?;
            transaction.commit()?;
        }

        let error = match Ledger::open(&database) {
            Ok(_) => anyhow::bail!("a pre-barrier v6 ledger migrated without authorization"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("explicit privacy migration"));
        assert!(error.to_string().contains("source files are missing"));
        let untouched = Connection::open(&database)?;
        let (sources, observations): (i64, i64) = untouched.query_row(
            "SELECT (SELECT COUNT(*) FROM source_files), (SELECT COUNT(*) FROM usage_observations)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!((sources, observations), (1, 1));
        drop(untouched);

        // The fixture never creates its source path. Once the explicitly
        // authorized privacy barrier invalidates this cache, that historical
        // accounting cannot be reconstructed from disk.
        let ledger = Ledger::open_for_v6_privacy_migration(&database)?;
        let (version, barrier, sources, observations): (i64, String, i64, i64) =
            ledger.connection.query_row(
                r#"SELECT
                       (SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'),
                       (SELECT value FROM meta WHERE key=?1),
                       (SELECT COUNT(*) FROM source_files),
                       (SELECT COUNT(*) FROM usage_observations)"#,
                [V7_PRIVACY_BARRIER_KEY],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(barrier, V7_PRIVACY_COMPLETE);
        assert_eq!(sources, 0);
        assert_eq!(observations, 0);
        let guard_count: i64 = ledger.connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'guard_%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            guard_count, 11,
            "eight write guards plus three schema guards"
        );

        let legacy = Connection::open(&database)?;
        let error = legacy
            .execute("UPDATE meta SET value='4' WHERE key='schema_version'", [])
            .expect_err("a delayed legacy migrator must not downgrade the barrier");
        assert!(error.to_string().contains("schema downgrade rejected"));
        let error = legacy
            .execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', '4')",
                [],
            )
            .expect_err("replacement must not bypass the downgrade barrier");
        assert!(error.to_string().contains("schema downgrade rejected"));
        drop(ledger);
        assert_database_family_excludes(&database, &[RAW_SESSION])?;
        Ok(())
    }

    #[test]
    fn simultaneous_v5_migrators_hash_each_identifier_exactly_once() -> Result<()> {
        const RAW_SESSION: &str = "simultaneous-v5-migration-session";
        const ROW_COUNT: usize = 10_000;

        let directory = tempdir()?;
        let database = directory.path().join("simultaneous-v5.sqlite");
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(
                Client::ClaudeCode,
                &directory.path().join("source.jsonl"),
                false,
            )?;
            let mut first = observation(RAW_SESSION, 1, 2);
            first.event_key = "event-0".to_string();
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &serde_json::Value::Null,
                observations: &[first],
                warnings: &[],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, 1, 0, "ok")?;
            emulate_legacy_schema(&ledger.connection, 5)?;
            ledger
                .connection
                .execute("UPDATE usage_observations SET session_id=?1", [RAW_SESSION])?;
            let transaction = ledger.connection.transaction()?;
            for ordinal in 1..ROW_COUNT {
                transaction.execute(
                    r#"INSERT INTO usage_observations(
                           source_file_id, event_key, client, session_id,
                           provider_message_id, occurred_at_utc, raw_model, provider,
                           input_tokens_total, input_tokens_uncached, input_tokens_cached,
                           cache_write_5m_tokens, cache_write_1h_tokens,
                           cache_write_unknown_tokens, output_tokens_total,
                           reasoning_output_tokens, web_search_requests, web_fetch_requests,
                           dimensions_json, quality_rank, coverage, source_locator,
                           parser_version, warnings_json, privacy_write_generation)
                       SELECT source_file_id, ?1, client, ?2, provider_message_id,
                              occurred_at_utc, raw_model, provider, input_tokens_total,
                              input_tokens_uncached, input_tokens_cached,
                              cache_write_5m_tokens, cache_write_1h_tokens,
                              cache_write_unknown_tokens, output_tokens_total,
                              reasoning_output_tokens, web_search_requests,
                              web_fetch_requests, dimensions_json, quality_rank,
                              coverage, source_locator, parser_version, warnings_json,
                              privacy_write_generation
                       FROM usage_observations WHERE id=1"#,
                    params![format!("event-{ordinal}"), RAW_SESSION],
                )?;
            }
            transaction.commit()?;
        }

        let start = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let database = database.clone();
            let start = Arc::clone(&start);
            workers.push(thread::spawn(move || -> Result<()> {
                start.wait();
                drop(Ledger::open(&database)?);
                Ok(())
            }));
        }
        start.wait();
        for worker in workers {
            match worker.join().expect("migration thread") {
                Ok(()) => {}
                Err(error) => {
                    let message = error.to_string();
                    assert!(
                        message.contains("WAL checkpoint")
                            || message.contains("database is locked")
                            || message.contains("database is busy"),
                        "simultaneous migration failed for a non-retryable reason: {message}"
                    );
                }
            }
        }

        // Once every contender has closed its connection, a retry must finish
        // any pending physical cleanup without replaying logical migration.
        let ledger = Ledger::open(&database)?;
        let expected = pseudonymous_session_id(Client::ClaudeCode, RAW_SESSION);
        let (count, distinct, stored, barrier): (i64, i64, String, String) =
            ledger.connection.query_row(
                r#"SELECT COUNT(*), COUNT(DISTINCT session_id), MIN(session_id),
                          (SELECT value FROM meta WHERE key=?1)
                   FROM usage_observations"#,
                [V7_PRIVACY_BARRIER_KEY],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(count, ROW_COUNT as i64);
        assert_eq!(distinct, 1);
        assert_eq!(stored, expected);
        assert_eq!(barrier, V7_PRIVACY_COMPLETE);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn ledger_open_hardens_database_directory_and_sqlite_files() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir()?;
        let app_dir = directory.path().join("token-ledger");
        std::fs::create_dir(&app_dir)?;
        std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o777))?;
        let database = app_dir.join("ledger.sqlite3");
        let _ledger = Ledger::open(&database)?;

        assert_eq!(
            std::fs::metadata(&app_dir)?.permissions().mode() & 0o777,
            0o700
        );
        for path in [
            database.clone(),
            sqlite_sidecar_path(&database, "-wal"),
            sqlite_sidecar_path(&database, "-shm"),
        ] {
            if path.try_exists()? {
                assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
            }
        }
        Ok(())
    }

    #[test]
    fn pending_v6_physical_cleanup_retries_without_rehashing_rows() -> Result<()> {
        const LOOKALIKE: &str = "tlses_eeeeeeeeeeeeeeeeeeeeeeee";

        let directory = tempdir()?;
        let database = directory.path().join("pending-cleanup.sqlite");
        let source_path = directory.path().join("source.jsonl");
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(Client::ClaudeCode, &source_path, false)?;
            let value = observation("initial", 1, 2);
            let state = serde_json::Value::Null;
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[value],
                warnings: &[],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, 1, 0, "ok")?;
            emulate_legacy_schema(&ledger.connection, 5)?;
            ledger
                .connection
                .execute("UPDATE usage_observations SET session_id=?1", [LOOKALIKE])?;
            ledger
                .connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        }

        // Hold an old WAL snapshot so the logical migration can commit but its
        // required TRUNCATE checkpoint reports busy.
        let blocker = Connection::open(&database)?;
        blocker.execute_batch("BEGIN")?;
        let _: i64 = blocker.query_row("SELECT COUNT(*) FROM usage_observations", [], |row| {
            row.get(0)
        })?;
        let error = match Ledger::open(&database) {
            Ok(_) => anyhow::bail!("privacy cleanup unexpectedly completed with a WAL reader"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("WAL checkpoint"));

        let inspection = Connection::open(&database)?;
        let (version, state, once_scrubbed): (i64, String, String) = inspection.query_row(
            r#"SELECT
                   (SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'),
                   (SELECT value FROM meta WHERE key=?1),
                   (SELECT session_id FROM usage_observations LIMIT 1)"#,
            [V7_PRIVACY_BARRIER_KEY],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(state, V7_PRIVACY_PENDING);
        assert_eq!(
            once_scrubbed,
            pseudonymous_session_id(Client::ClaudeCode, LOOKALIKE)
        );
        drop(inspection);
        blocker.execute_batch("ROLLBACK")?;
        drop(blocker);

        let ledger = Ledger::open(&database)?;
        let (state, after_retry): (String, String) = ledger.connection.query_row(
            r#"SELECT
                   (SELECT value FROM meta WHERE key=?1),
                   (SELECT session_id FROM usage_observations LIMIT 1)"#,
            [V7_PRIVACY_BARRIER_KEY],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(state, V7_PRIVACY_COMPLETE);
        assert_eq!(
            after_retry, once_scrubbed,
            "retry must not rehash logical rows"
        );
        drop(ledger);
        assert_database_family_excludes(&database, &[LOOKALIKE])?;
        Ok(())
    }

    #[test]
    fn v5_migration_processes_large_ledgers_in_bounded_keyset_batches() -> Result<()> {
        let directory = tempdir()?;
        let database = directory.path().join("large-batched-migration.sqlite");
        let source_path = directory.path().join("large-rollout.jsonl");
        let row_count = (PRIVACY_MIGRATION_BATCH_SIZE as usize * 2) + 17;
        let adapter_session = pseudonymous_session_id(Client::OpenaiCodex, "large-session");
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(Client::OpenaiCodex, &source_path, false)?;
            let observations = (1..=row_count)
                .map(|index| {
                    codex_observation(
                        &format!("legacy-boundary-{index}"),
                        &adapter_session,
                        index as u64,
                        &format!("line {index} @ byte {}", index * 32),
                        index as u64,
                    )
                })
                .collect::<Vec<_>>();
            let state = serde_json::json!({
                "session_ids_private": true,
                "logical_session_id": adapter_session,
                "usage_event_index": row_count
            });
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: row_count as u64,
                modified_ns: 1,
                checkpoint_offset: row_count as u64,
                checkpoint_line: row_count as u64,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &observations,
                warnings: &[],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, row_count as u64, 0, "ok")?;
            emulate_legacy_schema(&ledger.connection, 5)?;
        }

        let ledger = Ledger::open(&database)?;
        let observations: i64 =
            ledger
                .connection
                .query_row("SELECT COUNT(*) FROM usage_observations", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(observations, row_count as i64);
        let (aliases, canonical_keys, minimum_ordinal, maximum_ordinal): (i64, i64, i64, i64) =
            ledger.connection.query_row(
                r#"SELECT COUNT(*), COUNT(DISTINCT canonical_event_key),
                          MIN(usage_event_index), MAX(usage_event_index)
                   FROM codex_event_identity_aliases"#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(canonical_keys, row_count as i64);
        assert_eq!(
            aliases,
            row_count as i64 * CODEX_ALIAS_SCOPE_CANDIDATE_LIMIT as i64
        );
        assert_eq!(minimum_ordinal, 1);
        assert_eq!(maximum_ordinal, row_count as i64);
        assert_eq!(
            ledger.connection.query_row(
                r#"SELECT MAX(scope_count) FROM (
                       SELECT COUNT(*) AS scope_count
                       FROM codex_event_identity_aliases
                       GROUP BY source_file_id, canonical_event_key
                   )"#,
                [],
                |row| row.get::<_, i64>(0),
            )?,
            CODEX_ALIAS_SCOPE_CANDIDATE_LIMIT as i64,
            "alias growth is capped at three private scopes per migrated observation"
        );
        let checkpoint = ledger.source_checkpoint(&source_path)?.expect("checkpoint");
        assert_eq!(checkpoint.checkpoint_offset, 0);
        assert!(checkpoint.adapter_state.is_null());
        Ok(())
    }

    #[test]
    fn migrated_codex_alias_replay_survives_partial_reset_then_resume() -> Result<()> {
        let directory = tempdir()?;
        let database = directory.path().join("partial-alias-replay.sqlite");
        let source_path = directory.path().join("rollout.jsonl");
        let adapter_session = pseudonymous_session_id(Client::OpenaiCodex, "logical-session");
        let legacy = [
            codex_observation(
                "legacy-boundary-1",
                &adapter_session,
                1,
                "line 4 @ byte 20",
                2,
            ),
            codex_observation(
                "legacy-boundary-2",
                &adapter_session,
                2,
                "line 8 @ byte 80",
                3,
            ),
        ];
        let legacy_keys = legacy
            .iter()
            .map(UsageObservation::canonical_event_id)
            .collect::<Vec<_>>();
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(Client::OpenaiCodex, &source_path, false)?;
            let state = serde_json::json!({
                "session_ids_private": true,
                "logical_session_id": adapter_session,
                "usage_event_index": 2
            });
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 100,
                modified_ns: 1,
                checkpoint_offset: 100,
                checkpoint_line: 10,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &legacy,
                warnings: &[],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, 2, 0, "ok")?;
            emulate_legacy_schema(&ledger.connection, 5)?;
        }

        let mut ledger = Ledger::open(&database)?;
        let checkpoint = ledger.source_checkpoint(&source_path)?.expect("checkpoint");
        let first = codex_observation(
            "new-adapter-boundary-1",
            &adapter_session,
            1,
            "line 4 @ byte 20",
            2,
        );
        let state_one = serde_json::json!({
            "session_ids_private": true,
            "logical_session_id": adapter_session,
            "usage_event_index": 1
        });
        let first_run = ledger.start_scan("test")?;
        ledger.apply_source_update(SourceUpdate {
            source_id: checkpoint.id,
            reset_observations: true,
            file_size: 100,
            modified_ns: 2,
            checkpoint_offset: 40,
            checkpoint_line: 4,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state_one,
            observations: &[first],
            warnings: &[],
            scan_run_id: first_run,
        })?;
        ledger.finish_scan(first_run, 1, 1, 0, "partial")?;
        assert_eq!(
            ledger.connection.query_row(
                "SELECT COUNT(*) FROM codex_event_identity_replays",
                [],
                |row| row.get::<_, i64>(0),
            )?,
            1
        );

        // A resource-limited continuation is reset=false and may have a
        // different byte locator. The persisted replay marker permits the
        // stable session-scope + ordinal bridge to recover the legacy key.
        let second = codex_observation(
            "new-adapter-boundary-2",
            &adapter_session,
            2,
            "line 88 @ byte 880",
            3,
        );
        let state_two = serde_json::json!({
            "session_ids_private": true,
            "logical_session_id": adapter_session,
            "usage_event_index": 2
        });
        let second_run = ledger.start_scan("test")?;
        ledger.apply_source_update(SourceUpdate {
            source_id: checkpoint.id,
            reset_observations: false,
            file_size: 100,
            modified_ns: 2,
            checkpoint_offset: 100,
            checkpoint_line: 100,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state_two,
            observations: &[second],
            warnings: &[],
            scan_run_id: second_run,
        })?;
        ledger.finish_scan(second_run, 1, 1, 0, "ok")?;
        let mut stored = ledger
            .connection
            .prepare("SELECT event_key FROM usage_observations ORDER BY event_key")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut expected = legacy_keys.clone();
        stored.sort();
        expected.sort();
        assert_eq!(stored, expected);
        assert_eq!(
            ledger.connection.query_row(
                "SELECT COUNT(*) FROM codex_event_identity_replays",
                [],
                |row| row.get::<_, i64>(0),
            )?,
            0
        );

        // Once replay completes, an ordinary append cannot consult legacy
        // aliases even if it deliberately reuses an old ordinal.
        let appended = codex_observation(
            "truly-new-boundary",
            &adapter_session,
            2,
            "line 99 @ byte 990",
            4,
        );
        let appended_key = appended.canonical_event_id();
        let append_run = ledger.start_scan("test")?;
        ledger.apply_source_update(SourceUpdate {
            source_id: checkpoint.id,
            reset_observations: false,
            file_size: 120,
            modified_ns: 3,
            checkpoint_offset: 120,
            checkpoint_line: 120,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state_two,
            observations: &[appended],
            warnings: &[],
            scan_run_id: append_run,
        })?;
        assert!(ledger.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM usage_observations WHERE event_key=?1)",
            [appended_key],
            |row| row.get::<_, bool>(0),
        )?);
        Ok(())
    }

    #[test]
    fn copied_codex_source_first_seen_after_migration_uses_global_aliases() -> Result<()> {
        let directory = tempdir()?;
        let database = directory.path().join("global-copy-alias.sqlite");
        let original_path = directory.path().join("original.jsonl");
        let copied_path = directory.path().join("copied-after-migration.jsonl");
        let adapter_session = pseudonymous_session_id(Client::OpenaiCodex, "logical-session");
        let legacy = [
            codex_observation(
                "legacy-boundary-1",
                &adapter_session,
                1,
                "line 4 @ byte 20",
                2,
            ),
            codex_observation(
                "legacy-boundary-2",
                &adapter_session,
                2,
                "line 8 @ byte 80",
                3,
            ),
        ];
        let legacy_keys = legacy
            .iter()
            .map(UsageObservation::canonical_event_id)
            .collect::<Vec<_>>();
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(Client::OpenaiCodex, &original_path, false)?;
            let state = serde_json::json!({
                "session_ids_private": true,
                "logical_session_id": adapter_session,
                "usage_event_index": 2
            });
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 100,
                modified_ns: 1,
                checkpoint_offset: 100,
                checkpoint_line: 10,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &legacy,
                warnings: &[],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, 2, 0, "ok")?;
            emulate_legacy_schema(&ledger.connection, 5)?;
        }

        let mut ledger = Ledger::open(&database)?;
        let copied_source_id = ledger.ensure_source(Client::OpenaiCodex, &copied_path, false)?;
        let copied = [
            codex_observation("new-boundary-1", &adapter_session, 1, "line 4 @ byte 20", 2),
            // The exact first match anchors this newly discovered copy; the
            // second deliberately changed locator exercises global ordinal.
            codex_observation(
                "new-boundary-2",
                &adapter_session,
                2,
                "line 80 @ byte 800",
                3,
            ),
        ];
        let state = serde_json::json!({
            "session_ids_private": true,
            "logical_session_id": adapter_session,
            "usage_event_index": 2
        });
        let run = ledger.start_scan("test")?;
        ledger.apply_source_update(SourceUpdate {
            source_id: copied_source_id,
            reset_observations: true,
            file_size: 100,
            modified_ns: 2,
            checkpoint_offset: 100,
            checkpoint_line: 100,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state,
            observations: &copied,
            warnings: &[],
            scan_run_id: run,
        })?;
        ledger.finish_scan(run, 1, 2, 0, "ok")?;
        let mut canonical = ledger
            .canonical_events(None, None)?
            .into_iter()
            .map(|event| event.event_key)
            .collect::<Vec<_>>();
        let mut expected = legacy_keys;
        canonical.sort();
        expected.sort();
        assert_eq!(canonical, expected);
        assert_eq!(
            ledger.connection.query_row(
                "SELECT COUNT(*) FROM codex_event_identity_aliases",
                [],
                |row| row.get::<_, i64>(0),
            )?,
            2 * CODEX_ALIAS_SCOPE_CANDIDATE_LIMIT as i64,
            "new sources and ordinary writes must not grow migration aliases"
        );
        assert_eq!(
            ledger.connection.query_row(
                "SELECT COUNT(*) FROM codex_event_identity_replays",
                [],
                |row| row.get::<_, i64>(0),
            )?,
            0
        );
        Ok(())
    }

    #[test]
    fn unanchored_shifted_codex_copy_rolls_back_instead_of_double_counting() -> Result<()> {
        let directory = tempdir()?;
        let database = directory.path().join("unanchored-copy.sqlite");
        let original_path = directory.path().join("original.jsonl");
        let shifted_path = directory.path().join("shifted-copy.jsonl");
        let adapter_session = pseudonymous_session_id(Client::OpenaiCodex, "logical-session");
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(Client::OpenaiCodex, &original_path, false)?;
            let legacy = codex_observation(
                "legacy-boundary",
                &adapter_session,
                1,
                "line 4 @ byte 20",
                2,
            );
            let state = serde_json::json!({
                "session_ids_private": true,
                "logical_session_id": adapter_session,
                "usage_event_index": 1
            });
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 100,
                modified_ns: 1,
                checkpoint_offset: 100,
                checkpoint_line: 10,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[legacy],
                warnings: &[],
                scan_run_id: run,
            })?;
            ledger.finish_scan(run, 1, 1, 0, "ok")?;
            emulate_legacy_schema(&ledger.connection, 5)?;
        }

        let mut ledger = Ledger::open(&database)?;
        let shifted_source_id = ledger.ensure_source(Client::OpenaiCodex, &shifted_path, false)?;
        let shifted = codex_observation(
            "new-adapter-boundary",
            &adapter_session,
            1,
            "line 40 @ byte 200",
            2,
        );
        let state = serde_json::json!({
            "session_ids_private": true,
            "logical_session_id": adapter_session,
            "usage_event_index": 1
        });
        let run = ledger.start_scan("test")?;
        let error = ledger
            .apply_source_update(SourceUpdate {
                source_id: shifted_source_id,
                reset_observations: true,
                file_size: 100,
                modified_ns: 2,
                checkpoint_offset: 100,
                checkpoint_line: 100,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[shifted],
                warnings: &[],
                scan_run_id: run,
            })
            .expect_err("an unanchored global ordinal must fail closed");
        assert!(error.to_string().contains("unanchored migrated Codex"));
        assert_eq!(ledger.canonical_events(None, None)?.len(), 1);
        assert_eq!(
            ledger.connection.query_row(
                "SELECT COUNT(*) FROM usage_observations WHERE source_file_id=?1",
                [shifted_source_id],
                |row| row.get::<_, i64>(0),
            )?,
            0
        );
        assert_eq!(
            ledger.connection.query_row(
                "SELECT COUNT(*) FROM codex_event_identity_replays",
                [],
                |row| row.get::<_, i64>(0),
            )?,
            0,
            "the replay marker must roll back with the rejected source update"
        );
        Ok(())
    }

    #[test]
    fn ambiguous_migrated_alias_rolls_back_source_rebuild() -> Result<()> {
        let directory = tempdir()?;
        let database = directory.path().join("ambiguous-alias.sqlite");
        let source_path = directory.path().join("rollout.jsonl");
        let mut ledger = Ledger::open(&database)?;
        let first_run = ledger.start_scan("test")?;
        let source_id = ledger.ensure_source(Client::OpenaiCodex, &source_path, false)?;
        let state = serde_json::json!({
            "session_ids_private": true,
            "logical_session_id": pseudonymous_session_id(Client::OpenaiCodex, "session")
        });
        let mut existing = observation("session", 10, 2);
        existing.client = Client::OpenaiCodex;
        existing.event_key = "existing-boundary".to_string();
        existing.session_id = pseudonymous_session_id(Client::OpenaiCodex, "session");
        existing.provider = "openai".to_string();
        existing.raw_model = "gpt-5.4".to_string();
        existing.source_locator = "line 1 @ byte 0".to_string();
        ledger.apply_source_update(SourceUpdate {
            source_id,
            reset_observations: false,
            file_size: 1,
            modified_ns: 1,
            checkpoint_offset: 1,
            checkpoint_line: 1,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state,
            observations: &[existing.clone()],
            warnings: &[],
            scan_run_id: first_run,
        })?;
        ledger.finish_scan(first_run, 1, 1, 0, "ok")?;
        let original_event = existing.canonical_event_id();
        let persisted_session = pseudonymous_session_id(Client::OpenaiCodex, &existing.session_id);
        let session_scope = codex_session_scope(&persisted_session);
        for (ordinal, locator) in [(1, "line 8 @ byte 80"), (2, "line 9 @ byte 90")] {
            ledger.connection.execute(
                r#"INSERT INTO codex_event_identity_aliases(
                       source_file_id, canonical_event_key, session_scope,
                       source_locator, usage_event_index
                   ) VALUES (?1, ?2, ?3, ?4, ?5)"#,
                params![
                    source_id,
                    stable_id(&["ambiguous-legacy", &ordinal.to_string()]),
                    session_scope,
                    locator,
                    1_i64,
                ],
            )?;
        }

        let second_run = ledger.start_scan("test")?;
        let mut rebuilt = existing;
        rebuilt.event_key = "new-boundary".to_string();
        rebuilt.usage_event_index = Some(1);
        let error = ledger
            .apply_source_update(SourceUpdate {
                source_id,
                reset_observations: true,
                file_size: 1,
                modified_ns: 2,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[rebuilt],
                warnings: &[],
                scan_run_id: second_run,
            })
            .expect_err("ambiguous fallback must roll back");
        assert!(error.to_string().contains("ambiguous migrated Codex"));
        ledger.finish_scan(second_run, 0, 0, 1, "failed")?;
        let stored: Vec<String> = ledger
            .connection
            .prepare("SELECT event_key FROM usage_observations")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(stored, vec![original_event]);
        assert_eq!(ledger.canonical_events(None, None)?.len(), 1);
        let aliases: i64 = ledger.connection.query_row(
            "SELECT COUNT(*) FROM codex_event_identity_aliases",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(aliases, 2, "normal writes must not grow alias history");
        Ok(())
    }

    #[test]
    fn incremental_cache_classification_replaces_unknown_tokens() -> Result<()> {
        let dir = tempdir()?;
        let mut ledger = Ledger::open(&dir.path().join("test.sqlite"))?;
        let path = dir.path().join("session.jsonl");
        let source_id = ledger.ensure_source(Client::ClaudeCode, &path, false)?;
        let run = ledger.start_scan("test")?;
        let state = serde_json::Value::Null;

        let mut unclassified = observation("session", 10, 1);
        unclassified.usage = UsageVector {
            input_tokens_total: 10,
            input_tokens_uncached: 2,
            input_tokens_cached: 1,
            cache_write_unknown_tokens: 7,
            output_tokens_total: 1,
            ..Default::default()
        };
        unclassified.coverage = CoverageStatus::PartialKnown;
        unclassified.dimensions.cache_write_data_complete = Some(false);
        unclassified.warnings = vec!["cache-write TTL classification is incomplete".into()];
        ledger.apply_source_update(SourceUpdate {
            source_id,
            reset_observations: false,
            file_size: 1,
            modified_ns: 1,
            checkpoint_offset: 1,
            checkpoint_line: 1,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state,
            observations: &[unclassified],
            warnings: &[],
            scan_run_id: run,
        })?;

        let mut classified = observation("session", 10, 1);
        classified.usage = UsageVector {
            input_tokens_total: 10,
            input_tokens_uncached: 2,
            input_tokens_cached: 1,
            cache_write_5m_tokens: 5,
            cache_write_1h_tokens: 2,
            output_tokens_total: 1,
            ..Default::default()
        };
        classified.coverage = CoverageStatus::CompleteKnown;
        classified.dimensions.cache_write_data_complete = Some(true);
        ledger.apply_source_update(SourceUpdate {
            source_id,
            reset_observations: false,
            file_size: 2,
            modified_ns: 2,
            checkpoint_offset: 2,
            checkpoint_line: 2,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state,
            observations: &[classified],
            warnings: &[],
            scan_run_id: run,
        })?;

        let events = ledger.canonical_events(None, None)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].usage.input_tokens_total, 10);
        assert_eq!(events[0].usage.cache_write_5m_tokens, 5);
        assert_eq!(events[0].usage.cache_write_1h_tokens, 2);
        assert_eq!(events[0].usage.cache_write_unknown_tokens, 0);
        assert_eq!(events[0].coverage, CoverageStatus::CompleteKnown);
        assert_eq!(events[0].dimensions.cache_write_data_complete, Some(true));
        Ok(())
    }

    #[test]
    fn canonicalization_uses_identity_and_dimensions_from_best_observation() -> Result<()> {
        let dir = tempdir()?;
        let mut ledger = Ledger::open(&dir.path().join("test.sqlite"))?;
        let run = ledger.start_scan("test")?;
        let state = serde_json::Value::Null;

        let mut partial = observation("fallback-session", 10, 12);
        partial.raw_model = "unknown".into();
        partial.coverage = CoverageStatus::PartialKnown;
        let mut complete = observation("real-session", 20, 10);
        complete.raw_model = "claude-sonnet-4-6".into();
        complete.coverage = CoverageStatus::CompleteKnown;
        complete.dimensions.speed = Some("fast".into());

        for (name, value) in [("partial", partial), ("complete", complete)] {
            let path = dir.path().join(format!("{name}.jsonl"));
            let source_id = ledger.ensure_source(Client::ClaudeCode, &path, false)?;
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[value],
                warnings: &[],
                scan_run_id: run,
            })?;
        }

        let events = ledger.canonical_events(None, None)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].occurred_at.timestamp(), 10);
        assert_eq!(events[0].usage.output_tokens_total, 12);
        assert_eq!(
            events[0].session_id,
            pseudonymous_session_id(Client::ClaudeCode, "real-session")
        );
        assert_eq!(events[0].raw_model, "claude-sonnet-4-6");
        assert_eq!(events[0].dimensions.speed.as_deref(), Some("fast"));
        assert_eq!(events[0].coverage, CoverageStatus::CompleteKnown);
        Ok(())
    }

    #[test]
    fn purge_removes_rows_and_scrubs_database_and_wal() -> Result<()> {
        const PRIVATE_MARKER: &str = "private-session-marker-5f4ea3";

        let dir = tempdir()?;
        let database = dir.path().join("test.sqlite");
        let mut ledger = Ledger::open(&database)?;
        let run = ledger.start_scan("test")?;
        let source_path = dir.path().join(format!("{PRIVATE_MARKER}.jsonl"));
        let source_id = ledger.ensure_source(Client::ClaudeCode, &source_path, false)?;
        let state = serde_json::Value::Null;
        let mut value = observation(PRIVATE_MARKER, 10, 1);
        value.session_id = PRIVATE_MARKER.into();
        value.source_locator = format!("C:\\private\\{PRIVATE_MARKER}.jsonl:line 1 @ byte 0");
        ledger.apply_source_update(SourceUpdate {
            source_id,
            reset_observations: false,
            file_size: 1,
            modified_ns: 1,
            checkpoint_offset: 1,
            checkpoint_line: 1,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state,
            observations: &[value],
            warnings: &[],
            scan_run_id: run,
        })?;
        assert!(ledger.source_checkpoint(&source_path)?.is_some());
        assert!(
            !ledger.source_rows()?[0]
                .path
                .to_string_lossy()
                .contains(PRIVATE_MARKER)
        );
        let stored_locator: String = ledger.connection.query_row(
            "SELECT source_locator FROM usage_observations",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(stored_locator, "line 1 @ byte 0");
        ledger.connection.execute(
            r#"INSERT INTO codex_event_identity_aliases(
                   source_file_id, canonical_event_key, session_scope,
                   source_locator, usage_event_index
               ) VALUES (?1, ?2, ?3, ?4, ?5)"#,
            params![
                source_id,
                "evt_purge-cascade-test",
                codex_session_scope("tlses_purge-cascade-test"),
                "line 1 @ byte 0",
                1_i64,
            ],
        )?;

        ledger.purge()?;
        let stats = ledger.stats()?;
        assert_eq!(stats.sources, 0);
        assert_eq!(stats.observations, 0);
        assert_eq!(stats.canonical_events, 0);
        assert_eq!(stats.warnings, 0);
        assert_eq!(
            ledger.connection.query_row(
                "SELECT COUNT(*) FROM codex_event_identity_aliases",
                [],
                |row| row.get::<_, i64>(0),
            )?,
            0,
            "purging source rows must cascade to migration-only identity aliases"
        );

        for path in [&database, &database.with_extension("sqlite-wal")] {
            if path.exists() {
                let bytes = std::fs::read(path)?;
                assert!(
                    !bytes
                        .windows(PRIVATE_MARKER.len())
                        .any(|window| window == PRIVATE_MARKER.as_bytes()),
                    "private marker remained recoverable in {}",
                    path.display()
                );
            }
        }
        Ok(())
    }

    #[test]
    fn v1_migration_pseudonymizes_paths_and_sanitizes_locators() -> Result<()> {
        const PRIVATE_PATH: &str = "private-workspace-marker-91d77c";

        let dir = tempdir()?;
        let database = dir.path().join("test.sqlite");
        let source_path = dir.path().join(PRIVATE_PATH).join("session.jsonl");
        {
            let mut ledger = Ledger::open(&database)?;
            let run = ledger.start_scan("test")?;
            let source_id = ledger.ensure_source(Client::ClaudeCode, &source_path, false)?;
            let state = serde_json::Value::Null;
            let mut value = observation("session", 10, 1);
            value.source_locator =
                format!("C:\\users\\private\\{PRIVATE_PATH}\\session.jsonl:line 9 @ byte 42");
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[value],
                warnings: &[],
                scan_run_id: run,
            })?;

            // Recreate the privacy-relevant shape of a schema-v1 ledger.
            emulate_legacy_schema(&ledger.connection, 1)?;
            ledger.connection.execute(
                "UPDATE source_files SET path=?1 WHERE id=?2",
                params![source_path.to_string_lossy(), source_id],
            )?;
            ledger.connection.execute(
                "UPDATE usage_observations SET source_locator=?1",
                [format!(
                    "C:\\users\\private\\{PRIVATE_PATH}\\session.jsonl:line 9 @ byte 42"
                )],
            )?;
        }

        let ledger = Ledger::open(&database)?;
        assert!(ledger.source_checkpoint(&source_path)?.is_some());
        assert!(
            !ledger.source_rows()?[0]
                .path
                .to_string_lossy()
                .contains(PRIVATE_PATH)
        );
        let stored_locator: String = ledger.connection.query_row(
            "SELECT source_locator FROM usage_observations",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(stored_locator, "line 9 @ byte 42");
        Ok(())
    }

    #[test]
    fn coverage_distinguishes_no_sources_from_scanned_sources_without_usage() -> Result<()> {
        let mut ledger = Ledger::open_in_memory()?;
        let empty = ledger.coverage_snapshot()?;
        assert!(empty.last_scan.is_none());
        assert!(empty.as_of.is_none());
        assert!(!empty.provisional);
        assert_eq!(empty.active_or_volatile_source_count, 0);
        assert!(empty.warning_counts.is_empty());
        assert_eq!(empty.clients.len(), Client::ALL.len());
        assert!(empty.clients.iter().all(|client| {
            client.window_status == CoverageWindowStatus::NoSources
                && client.source_count == 0
                && client.earliest_canonical_event.is_none()
                && client.latest_canonical_event.is_none()
        }));

        let run = ledger.start_scan("incremental")?;
        let running = ledger.coverage_snapshot()?;
        assert!(running.provisional);
        assert!(running.as_of.is_none());
        assert_eq!(
            running.last_scan.as_ref().map(|scan| scan.status.as_str()),
            Some("running")
        );
        let source_id = ledger.ensure_source(
            Client::OpenaiCodex,
            Path::new("C:\\private\\rollout.jsonl"),
            false,
        )?;
        let state = serde_json::Value::Null;
        ledger.apply_source_update(SourceUpdate {
            source_id,
            reset_observations: false,
            file_size: 0,
            modified_ns: 0,
            checkpoint_offset: 0,
            checkpoint_line: 0,
            checkpoint_hash: "",
            head_hash: "",
            adapter_state: &state,
            observations: &[],
            warnings: &[],
            scan_run_id: run,
        })?;
        ledger.finish_scan(run, 1, 0, 0, "ok")?;

        let coverage = ledger.coverage_snapshot()?;
        let codex = coverage
            .clients
            .iter()
            .find(|value| value.client == Client::OpenaiCodex)
            .unwrap();
        assert_eq!(codex.window_status, CoverageWindowStatus::NoObservations);
        assert_eq!(codex.source_count, 1);
        assert_eq!(codex.observation_count, 0);
        assert_eq!(codex.canonical_event_count, 0);
        assert_eq!(codex.warning_count, 0);
        assert_eq!(
            codex
                .last_successful_source_scan
                .as_ref()
                .map(|scan| scan.status.as_str()),
            Some("ok")
        );
        assert!(codex.earliest_canonical_event.is_none());
        assert!(codex.latest_canonical_event.is_none());

        let last_scan = coverage.last_scan.unwrap();
        assert_eq!(last_scan.mode, "incremental");
        assert_eq!(last_scan.status, "ok");
        assert!(last_scan.completed_at.is_some());
        assert_eq!(last_scan.source_count, 1);
        assert_eq!(last_scan.observation_count, 0);
        Ok(())
    }

    #[test]
    fn coverage_reports_canonical_window_and_sanitized_warning_rollups() -> Result<()> {
        const PRIVATE_MARKER: &str = "private-warning-message-1b50c2";

        let dir = tempdir()?;
        let mut ledger = Ledger::open(&dir.path().join("test.sqlite"))?;
        let run = ledger.start_scan("full")?;
        let state = serde_json::Value::Null;
        let warning = ScanWarning::new(
            "parse_issue",
            format!("transcript C:\\private\\{PRIVATE_MARKER} could not be parsed"),
        )
        .at(format!("C:\\private\\{PRIVATE_MARKER}:line 8 @ byte 90"));

        for (name, timestamp, output) in [("one", 20, 5), ("two", 10, 7)] {
            let path = dir
                .path()
                .join(PRIVATE_MARKER)
                .join(format!("{name}.jsonl"));
            let source_id = ledger.ensure_source(Client::ClaudeCode, &path, false)?;
            let mut value = observation(name, timestamp, output);
            value.source_locator = format!(
                "C:\\private\\{PRIVATE_MARKER}\\{name}.jsonl:line 4 @ byte 12 trailing C:\\leak"
            );
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[value],
                warnings: std::slice::from_ref(&warning),
                scan_run_id: run,
            })?;

            if name == "one" {
                let mut later = observation("later-session", 30, 9);
                later.event_key = "later".into();
                ledger.apply_source_update(SourceUpdate {
                    source_id,
                    reset_observations: false,
                    file_size: 2,
                    modified_ns: 2,
                    checkpoint_offset: 2,
                    checkpoint_line: 2,
                    checkpoint_hash: "",
                    head_hash: "",
                    adapter_state: &state,
                    observations: &[later],
                    warnings: &[],
                    scan_run_id: run,
                })?;
            }
        }
        ledger.record_scan_warning(
            run,
            None,
            &ScanWarning::new(
                format!("unsafe C:\\{PRIVATE_MARKER}"),
                format!("do not expose {PRIVATE_MARKER}"),
            ),
        )?;
        ledger.finish_scan(run, 2, 3, 3, "ok")?;

        let snapshot = ledger.coverage_snapshot()?;
        let claude = snapshot
            .clients
            .iter()
            .find(|value| value.client == Client::ClaudeCode)
            .unwrap();
        assert_eq!(claude.window_status, CoverageWindowStatus::ObservedWindow);
        assert_eq!(claude.source_count, 2);
        assert_eq!(claude.observation_count, 3);
        assert_eq!(claude.canonical_event_count, 2);
        assert_eq!(claude.warning_count, 2);
        assert_eq!(
            claude.earliest_canonical_event.as_ref().unwrap().event_id,
            stable_id(&[Client::ClaudeCode.as_str(), "shared"])
        );
        assert_eq!(
            claude
                .earliest_canonical_event
                .as_ref()
                .unwrap()
                .occurred_at
                .timestamp(),
            10
        );
        assert_eq!(
            claude.latest_canonical_event.as_ref().unwrap().event_id,
            stable_id(&[Client::ClaudeCode.as_str(), "later"])
        );
        assert_eq!(
            claude
                .latest_canonical_event
                .as_ref()
                .unwrap()
                .occurred_at
                .timestamp(),
            30
        );

        assert_eq!(
            snapshot.warning_counts,
            vec![
                WarningCodeCount {
                    client: None,
                    code: "unknown_warning".into(),
                    count: 1,
                },
                WarningCodeCount {
                    client: Some(Client::ClaudeCode),
                    code: "parse_issue".into(),
                    count: 2,
                },
            ]
        );
        let json = serde_json::to_string(&snapshot)?;
        assert!(!json.contains(PRIVATE_MARKER));
        assert!(!json.contains("transcript"));
        Ok(())
    }

    #[test]
    fn event_provenance_is_complete_and_never_returns_paths_or_bodies() -> Result<()> {
        const PRIVATE_MARKER: &str = "private-provenance-marker-0d8f3e";

        let dir = tempdir()?;
        let mut ledger = Ledger::open(&dir.path().join("test.sqlite"))?;
        let run = ledger.start_scan("test")?;
        let state = serde_json::Value::Null;
        for (index, output) in [(1, 5), (2, 8)] {
            let path = dir
                .path()
                .join(PRIVATE_MARKER)
                .join(format!("source-{index}.jsonl"));
            let source_id = ledger.ensure_source(Client::ClaudeCode, &path, false)?;
            let mut value = observation(PRIVATE_MARKER, 10 + index, output);
            value.source_locator = format!(
                "C:\\private\\{PRIVATE_MARKER}\\source-{index}.jsonl:line {index} @ byte {} C:\\trailing-leak",
                index * 10
            );
            value.parser_version = if index == 1 {
                "claude-jsonl-v1".into()
            } else {
                format!("C:\\private\\{PRIVATE_MARKER}")
            };
            value.raw_model = if index == 1 {
                "claude-sonnet-4-6".into()
            } else {
                format!("C:\\private\\{PRIVATE_MARKER}")
            };
            value.warnings = vec![format!("raw transcript body {PRIVATE_MARKER}")];
            ledger.apply_source_update(SourceUpdate {
                source_id,
                reset_observations: false,
                file_size: 1,
                modified_ns: 1,
                checkpoint_offset: 1,
                checkpoint_line: 1,
                checkpoint_hash: "",
                head_hash: "",
                adapter_state: &state,
                observations: &[value],
                warnings: &[],
                scan_run_id: run,
            })?;
        }

        let event_id = stable_id(&[Client::ClaudeCode.as_str(), "shared"]);
        let provenance = ledger.event_provenance(&event_id)?.unwrap();
        assert_eq!(provenance.event_id, event_id);
        assert_eq!(provenance.client, Client::ClaudeCode);
        assert_eq!(provenance.observation_count, 2);
        assert_eq!(provenance.deduplicated_observation_count, 1);
        assert_eq!(provenance.source_count, 2);
        assert_eq!(provenance.observations.len(), 2);
        assert!(provenance.observations.iter().all(|observation| {
            observation.pseudonymous_source_id.starts_with("src_")
                && observation.pseudonymous_source_id.len() == 28
        }));
        assert_ne!(
            provenance.observations[0].pseudonymous_source_id,
            provenance.observations[1].pseudonymous_source_id
        );
        assert_eq!(
            provenance.observations[0].source_locator,
            "line 1 @ byte 10"
        );
        assert_eq!(provenance.observations[0].parser_version, "claude-jsonl-v1");
        assert_eq!(provenance.observations[0].raw_model, "claude-sonnet-4-6");
        assert_eq!(provenance.observations[0].usage.output_tokens_total, 5);
        assert_eq!(provenance.observations[0].quality, UsageQuality::Exact);
        assert_eq!(provenance.observations[0].coverage, CoverageStatus::Unknown);
        assert_eq!(provenance.observations[1].parser_version, "unknown");
        assert_eq!(provenance.observations[1].raw_model, "unknown");

        let json = serde_json::to_string(&provenance)?;
        assert!(!json.contains(PRIVATE_MARKER));
        assert!(!json.contains("raw transcript body"));
        assert!(ledger.event_provenance("evt_does_not_exist")?.is_none());
        Ok(())
    }
}
