use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use rust_decimal::Decimal;

use crate::model::{
    CanonicalEvent, Client, ClientCoverageSnapshot, CoverageEventBoundary, CoverageStatus,
    CoverageWindowStatus, EventProvenance, LedgerCoverageSnapshot, ObservationProvenance,
    PricingDimensions, ScanRunSnapshot, ScanWarning, SuccessfulSourceScan, UsageObservation,
    UsageQuality, UsageVector, WarningCodeCount,
};
use crate::reconcile::{
    ImportReceipt, ParsedReconciliationImport, ReconciliationCounters, ReconciliationImportRecord,
    ReconciliationRouting, StoredReconciliationBucket,
};

const SCHEMA_VERSION: i64 = 4;
const SCAN_HEARTBEAT_STALE_AFTER_SECS: i64 = 15 * 60;

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
        if let Some(parent) = path.parent().filter(|value| !value.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("failed to open ledger database {}", path.display()))?;
        Self::from_connection(connection, path.to_path_buf(), "WAL")
    }

    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().context("failed to open in-memory ledger")?;
        Self::from_connection(connection, PathBuf::from(":memory:"), "MEMORY")
    }

    fn from_connection(connection: Connection, path: PathBuf, journal_mode: &str) -> Result<Self> {
        connection.pragma_update(None, "journal_mode", journal_mode)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "busy_timeout", 5_000_i64)?;
        let mut ledger = Self { connection, path };
        ledger.migrate()?;
        Ok(ledger)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&mut self) -> Result<()> {
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
                last_scan_at TEXT,
                last_status TEXT NOT NULL DEFAULT 'new'
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
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS reconciliation_imports (
                id INTEGER PRIMARY KEY,
                content_digest TEXT NOT NULL UNIQUE,
                source_kind TEXT NOT NULL,
                adapter TEXT NOT NULL,
                provider TEXT,
                imported_at TEXT NOT NULL,
                byte_count INTEGER NOT NULL,
                bucket_count INTEGER NOT NULL
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

        let existing: Option<i64> = self
            .connection
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            None => {
                self.connection.execute(
                    "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(version) if version == SCHEMA_VERSION => {}
            Some(1) => {
                self.migrate_v1_to_v2()?;
                self.migrate_v2_to_v3()?;
                self.migrate_v3_to_v4()?;
            }
            Some(2) => {
                self.migrate_v2_to_v3()?;
                self.migrate_v3_to_v4()?;
            }
            Some(3) => self.migrate_v3_to_v4()?,
            Some(version) => anyhow::bail!(
                "database schema version {version} is unsupported by this binary (expected {SCHEMA_VERSION})"
            ),
        }
        Ok(())
    }

    fn migrate_v1_to_v2(&mut self) -> Result<()> {
        let transaction = self.connection.transaction()?;
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
        // Older core-generated discovery/source errors could contain absolute
        // paths. They are diagnostics rather than accounting facts, so remove
        // only those legacy rows during the privacy migration.
        transaction.execute(
            "DELETE FROM scan_warnings WHERE code IN ('discovery_failed', 'source_scan_failed')",
            [],
        )?;
        transaction.execute("UPDATE meta SET value=?1 WHERE key='schema_version'", ["2"])?;
        transaction.commit()?;
        Ok(())
    }

    fn migrate_v2_to_v3(&mut self) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let columns = {
            let mut statement = transaction.prepare("PRAGMA table_info(scan_runs)")?;
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<HashSet<_>>>()?
        };
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
        transaction.execute("UPDATE meta SET value=?1 WHERE key='schema_version'", ["3"])?;
        transaction.commit()?;
        Ok(())
    }

    fn migrate_v3_to_v4(&mut self) -> Result<()> {
        // The tables are created with IF NOT EXISTS in the common schema
        // bootstrap above. Advancing the marker in its own transaction makes
        // an interrupted v3 open safely retryable.
        let transaction = self.connection.transaction()?;
        transaction.execute("UPDATE meta SET value='4' WHERE key='schema_version'", [])?;
        transaction.commit()?;
        Ok(())
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
            params![now_text, mode],
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
        Ok(())
    }

    pub fn source_checkpoint(&self, path: &Path) -> Result<Option<SourceCheckpoint>> {
        let path_key = source_storage_key(path);
        self.connection
            .query_row(
                r#"SELECT id, client, path, compressed, file_size, modified_ns,
                          checkpoint_offset, checkpoint_line, checkpoint_hash, head_hash,
                          adapter_state
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
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn ensure_source(&self, client: Client, path: &Path, compressed: bool) -> Result<i64> {
        let path_key = source_storage_key(path);
        self.connection.execute(
            "INSERT INTO source_files(client, path, compressed) VALUES (?1, ?2, ?3) ON CONFLICT(path) DO UPDATE SET client=excluded.client, compressed=excluded.compressed",
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
        if update.reset_observations {
            transaction.execute(
                "DELETE FROM usage_observations WHERE source_file_id=?1",
                [update.source_id],
            )?;
        }
        for observation in update.observations {
            upsert_observation(&transaction, update.source_id, observation)?;
        }
        for warning in update.warnings {
            transaction.execute(
                "INSERT INTO scan_warnings(scan_run_id, source_file_id, code, message, locator, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
                    adapter_state=?7, last_scan_at=?8, last_status='ok'
                WHERE id=?9"#,
            params![
                to_i64(update.file_size)?,
                update.modified_ns,
                to_i64(update.checkpoint_offset)?,
                to_i64(update.checkpoint_line)?,
                update.checkpoint_hash,
                update.head_hash,
                serde_json::to_string(update.adapter_state)?,
                Utc::now().to_rfc3339(),
                update.source_id,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_scan_warning(
        &self,
        scan_run_id: i64,
        source_file_id: Option<i64>,
        warning: &ScanWarning,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO scan_warnings(scan_run_id, source_file_id, code, message, locator, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
               WHERE source.client=?1"#,
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
                    event_id: crate::model::stable_id(&[client.as_str(), &event_key]),
                    occurred_at: parse_datetime_sql(1, &occurred_at)?,
                })
            })
            .optional()
            .map_err(Into::into)
    }

    fn event_identity(&self, event_id: &str) -> Result<Option<(Client, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT client, event_key FROM usage_observations GROUP BY client, event_key",
        )?;
        let rows = statement.query_map([], |row| {
            let client_text: String = row.get(0)?;
            Ok((parse_client_sql(&client_text)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (client, event_key) = row?;
            if crate::model::stable_id(&[client.as_str(), &event_key]) == event_id {
                return Ok(Some((client, event_key)));
            }
        }
        Ok(None)
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
                      adapter_state
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
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, Option<String>, i64)> = transaction
            .query_row(
                r#"SELECT source_kind, adapter, provider, bucket_count
                   FROM reconciliation_imports WHERE content_digest=?1"#,
                [&import.content_digest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((source_kind, adapter, provider, bucket_count)) = existing {
            transaction.commit()?;
            return Ok(ImportReceipt {
                content_digest: import.content_digest.clone(),
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
                   byte_count, bucket_count
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            params![
                import.content_digest,
                import.source_kind,
                import.adapter,
                import.provider,
                imported_at,
                to_i64(import.byte_count)?,
                to_i64(import.buckets.len() as u64)?,
            ],
        )?;
        let import_id = transaction.last_insert_rowid();
        for (ordinal, bucket) in import.buckets.iter().enumerate() {
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
                    bucket.source_kind,
                    bucket.bucket_start.to_rfc3339(),
                    bucket.bucket_end.to_rfc3339(),
                    bucket.provider,
                    bucket.model,
                    optional_i64(bucket.counters.request_count)?,
                    optional_i64(bucket.counters.input_tokens_uncached)?,
                    optional_i64(bucket.counters.input_tokens_cached)?,
                    optional_i64(bucket.counters.cache_write_5m_tokens)?,
                    optional_i64(bucket.counters.cache_write_1h_tokens)?,
                    optional_i64(bucket.counters.cache_write_unknown_tokens)?,
                    optional_i64(bucket.counters.output_tokens)?,
                    bucket.provider_metered_usd.map(|value| value.to_string()),
                    bucket.routing.inference_geo,
                    bucket.routing.service_tier,
                    bucket.routing.provider_route,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(ImportReceipt {
            content_digest: import.content_digest.clone(),
            source_kind: import.source_kind.clone(),
            adapter: import.adapter.clone(),
            provider: import.provider.clone(),
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

fn upsert_observation(
    transaction: &Transaction<'_>,
    source_id: i64,
    observation: &UsageObservation,
) -> Result<()> {
    let usage = &observation.usage;
    transaction.execute(
        r#"INSERT INTO usage_observations(
                source_file_id, event_key, client, session_id, provider_message_id,
                occurred_at_utc, raw_model, provider,
                input_tokens_total, input_tokens_uncached, input_tokens_cached,
                cache_write_5m_tokens, cache_write_1h_tokens, cache_write_unknown_tokens,
                output_tokens_total, reasoning_output_tokens,
                web_search_requests, web_fetch_requests,
                dimensions_json, quality_rank, coverage,
                source_locator, parser_version, warnings_json
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                ?19, ?20, ?21, ?22, ?23, ?24
            )
            ON CONFLICT(source_file_id, event_key) DO UPDATE SET
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
            observation.event_key,
            observation.client.as_str(),
            observation.session_id,
            observation.provider_message_id,
            observation.occurred_at.to_rfc3339(),
            observation.raw_model,
            observation.provider,
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
            serde_json::to_string(&observation.dimensions)?,
            observation.quality.rank(),
            observation.coverage.as_str(),
            sanitize_source_locator(&observation.source_locator),
            observation.parser_version,
            serde_json::to_string(&observation.warnings)?,
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
        event_id: crate::model::stable_id(&[client.as_str(), &event_key]),
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

fn source_storage_key(path: &Path) -> String {
    crate::model::stable_id(&["source-file", &path.to_string_lossy()])
}

fn pseudonymous_source_id(source_storage_key: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"provenance-source\0");
    hasher.update(source_storage_key.as_bytes());
    format!("src_{}", &hex::encode(hasher.finalize())[..24])
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
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use tempfile::tempdir;

    fn observation(source: &str, timestamp: i64, output: u64) -> UsageObservation {
        UsageObservation {
            event_key: "shared".into(),
            client: Client::ClaudeCode,
            session_id: source.into(),
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
        assert_eq!(version, 4);
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
        let parsed = parse_import(
            include_bytes!("../tests/fixtures/openai_organization_usage.json"),
            ImportFormat::Openai,
        )?;
        ledger.store_reconciliation_import(&parsed)?;
        ledger
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(ledger);

        let bytes = std::fs::read(&database)?;
        for canary in [
            "project-private-canary",
            "user-private-canary",
            "key-private-canary",
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
        assert_eq!(events[0].session_id, "real-session");
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

        ledger.purge()?;
        let stats = ledger.stats()?;
        assert_eq!(stats.sources, 0);
        assert_eq!(stats.observations, 0);
        assert_eq!(stats.canonical_events, 0);
        assert_eq!(stats.warnings, 0);

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
            ledger
                .connection
                .execute("UPDATE meta SET value='1' WHERE key='schema_version'", [])?;
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
