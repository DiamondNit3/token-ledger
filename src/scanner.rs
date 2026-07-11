use std::collections::HashSet;
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::adapters::SourceAdapter;
use crate::config::Config;
use crate::db::{Ledger, SourceCheckpoint, SourceUpdate};
use crate::model::{
    Client, LineRecord, ParseBatch, ScanWarning, SourceSpec, UsageObservation, stable_id,
};

const LINE_BATCH_SIZE: usize = 512;
const CHECKPOINT_WINDOW: u64 = 4096;
const FINGERPRINT_WINDOW: u64 = 4096;
const SOURCE_STABILITY_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub clients: HashSet<Client>,
    pub since: Option<DateTime<Utc>>,
    pub full: bool,
    pub dry_run: bool,
}

impl ScanOptions {
    fn includes(&self, client: Client) -> bool {
        self.clients.is_empty() || self.clients.contains(&client)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    pub discovered_sources: u64,
    pub scanned_sources: u64,
    pub unchanged_sources: u64,
    pub observations: u64,
    pub warnings: u64,
    pub reset_sources: u64,
    pub as_of: Option<DateTime<Utc>>,
    pub active_or_volatile_source_count: u64,
    pub provisional: bool,
    pub dry_run: bool,
}

pub fn scan(
    ledger: &mut Ledger,
    config: &Config,
    adapters: &[Box<dyn SourceAdapter>],
    options: &ScanOptions,
) -> Result<ScanSummary> {
    let mut summary = ScanSummary {
        dry_run: options.dry_run,
        ..Default::default()
    };
    let mode = if options.dry_run {
        "dry_run"
    } else if options.full {
        "full"
    } else {
        "incremental"
    };
    let scan_run_id = if options.dry_run {
        None
    } else {
        Some(ledger.start_scan(mode)?)
    };
    let mut stable_sources = Vec::<(PathBuf, SourceFingerprint)>::new();
    let mut active_or_volatile_sources = HashSet::<PathBuf>::new();

    let result = (|| -> Result<()> {
        for adapter in adapters {
            if !options.includes(adapter.client()) {
                continue;
            }
            let mut sources = match adapter.discover(config) {
                Ok(sources) => sources,
                Err(_error) => {
                    let warning = ScanWarning::new(
                        "discovery_failed",
                        format!(
                            "{} source discovery failed; verify that its configured root is readable",
                            adapter.display_name()
                        ),
                    );
                    summary.warnings += 1;
                    if let Some(run_id) = scan_run_id {
                        ledger.record_scan_warning(run_id, None, &warning)?;
                    }
                    continue;
                }
            };
            sources.sort_by(|left, right| left.path.cmp(&right.path));
            sources.dedup_by(|left, right| left.path == right.path);
            summary.discovered_sources += sources.len() as u64;

            for source in sources {
                if let Some(run_id) = scan_run_id {
                    ledger.heartbeat_scan(run_id)?;
                }
                match scan_one(ledger, adapter.as_ref(), &source, options, scan_run_id) {
                    Ok(SourceOutcome::Unchanged {
                        fingerprint,
                        active_during_scan,
                    }) => {
                        summary.unchanged_sources += 1;
                        if active_during_scan {
                            active_or_volatile_sources.insert(source.path.clone());
                        }
                        stable_sources.push((source.path.clone(), fingerprint));
                    }
                    Ok(SourceOutcome::Scanned {
                        observations,
                        warnings,
                        reset,
                        fingerprint,
                        active_during_scan,
                    }) => {
                        summary.scanned_sources += 1;
                        summary.observations += observations;
                        summary.warnings += warnings;
                        summary.reset_sources += u64::from(reset);
                        if active_during_scan {
                            active_or_volatile_sources.insert(source.path.clone());
                        }
                        stable_sources.push((source.path.clone(), fingerprint));
                    }
                    Ok(SourceOutcome::Volatile) => {
                        active_or_volatile_sources.insert(source.path.clone());
                        summary.warnings += 1;
                        let warning = ScanWarning::new(
                            "source_volatile",
                            format!(
                                "{} source remained active during bounded stability retries; it will be retried on the next scan",
                                adapter.display_name()
                            ),
                        );
                        if let Some(run_id) = scan_run_id {
                            let source_id = ledger
                                .source_checkpoint(&source.path)?
                                .map(|checkpoint| checkpoint.id);
                            ledger.record_scan_warning(run_id, source_id, &warning)?;
                        }
                    }
                    Err(_error) => {
                        summary.warnings += 1;
                        let source_label = stable_id(&[
                            "source",
                            source.client.as_str(),
                            &source.path.to_string_lossy(),
                        ]);
                        let warning = ScanWarning::new(
                            "source_scan_failed",
                            format!(
                                "{} source {} could not be scanned",
                                adapter.display_name(),
                                &source_label[..16]
                            ),
                        );
                        if let Some(run_id) = scan_run_id {
                            let source_id = ledger
                                .source_checkpoint(&source.path)?
                                .map(|checkpoint| checkpoint.id);
                            ledger.record_scan_warning(run_id, source_id, &warning)?;
                        }
                    }
                }
            }
        }

        // A source can become active after its individual parse succeeds.
        // Revalidate every stable fingerprint at one common boundary so the
        // resulting `as_of` and provisional flag have useful semantics.
        for (index, (path, fingerprint)) in stable_sources.iter().enumerate() {
            if index % 32 == 0
                && let Some(run_id) = scan_run_id
            {
                ledger.heartbeat_scan(run_id)?;
            }
            if source_fingerprint(path).as_ref().ok() != Some(fingerprint) {
                active_or_volatile_sources.insert(path.clone());
            }
        }
        Ok(())
    })();

    let as_of = Utc::now();
    summary.as_of = Some(as_of);
    summary.active_or_volatile_source_count = active_or_volatile_sources.len() as u64;
    summary.provisional = result.is_err() || summary.active_or_volatile_source_count > 0;
    if let Some(run_id) = scan_run_id {
        let status = if result.is_ok() { "ok" } else { "failed" };
        ledger.finish_scan_snapshot(
            run_id,
            summary.scanned_sources,
            summary.observations,
            summary.warnings,
            status,
            summary.active_or_volatile_source_count,
            summary.provisional,
            as_of,
        )?;
    }
    result?;
    Ok(summary)
}

enum SourceOutcome {
    Unchanged {
        fingerprint: SourceFingerprint,
        active_during_scan: bool,
    },
    Scanned {
        observations: u64,
        warnings: u64,
        reset: bool,
        fingerprint: SourceFingerprint,
        active_during_scan: bool,
    },
    Volatile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceFingerprint {
    file_size: u64,
    modified_ns: i64,
    sample_hash: String,
}

enum PreparedSource {
    Unchanged,
    Scanned(PreparedSourceUpdate),
}

struct PreparedSourceUpdate {
    reset: bool,
    file_size: u64,
    modified_ns: i64,
    checkpoint_offset: u64,
    checkpoint_line: u64,
    checkpoint_hash: String,
    head_hash: String,
    persisted_state: Value,
    batch: ParseBatch,
}

fn scan_one(
    ledger: &mut Ledger,
    adapter: &dyn SourceAdapter,
    source: &SourceSpec,
    options: &ScanOptions,
    scan_run_id: Option<i64>,
) -> Result<SourceOutcome> {
    let mut active_during_scan = false;
    for attempt in 0..SOURCE_STABILITY_ATTEMPTS {
        let before = source_fingerprint(&source.path)
            .with_context(|| format!("failed to fingerprint {}", source.path.display()))?;
        let prepared = prepare_source(ledger, adapter, source, options, &before);
        let after = source_fingerprint(&source.path);

        let stable = after
            .as_ref()
            .is_ok_and(|fingerprint| fingerprint == &before);
        if !stable {
            active_during_scan = true;
            if attempt + 1 == SOURCE_STABILITY_ATTEMPTS {
                return Ok(SourceOutcome::Volatile);
            }
            continue;
        }

        let fingerprint = after.expect("stable fingerprint checked above");
        return match prepared? {
            PreparedSource::Unchanged => Ok(SourceOutcome::Unchanged {
                fingerprint,
                active_during_scan,
            }),
            PreparedSource::Scanned(prepared) => {
                let observation_count = prepared.batch.observations.len() as u64;
                let warning_count = prepared.batch.warnings.len() as u64;
                if !options.dry_run {
                    let run_id = scan_run_id.context("non-dry scan is missing a scan run")?;
                    // Re-check the lease after parsing. If a long-running parse
                    // was recovered as stale, the old writer must not commit
                    // its prepared observations after the replacement starts.
                    ledger.heartbeat_scan(run_id)?;
                    let source_id =
                        ledger.ensure_source(source.client, &source.path, source.compressed)?;
                    ledger.apply_source_update(SourceUpdate {
                        source_id,
                        reset_observations: prepared.reset,
                        file_size: prepared.file_size,
                        modified_ns: prepared.modified_ns,
                        checkpoint_offset: prepared.checkpoint_offset,
                        checkpoint_line: prepared.checkpoint_line,
                        checkpoint_hash: &prepared.checkpoint_hash,
                        head_hash: &prepared.head_hash,
                        adapter_state: &prepared.persisted_state,
                        observations: &prepared.batch.observations,
                        warnings: &prepared.batch.warnings,
                        scan_run_id: run_id,
                    })?;
                }
                Ok(SourceOutcome::Scanned {
                    observations: observation_count,
                    warnings: warning_count,
                    reset: prepared.reset,
                    fingerprint,
                    active_during_scan,
                })
            }
        };
    }
    unreachable!("bounded stability loop always returns")
}

fn prepare_source(
    ledger: &Ledger,
    adapter: &dyn SourceAdapter,
    source: &SourceSpec,
    options: &ScanOptions,
    fingerprint: &SourceFingerprint,
) -> Result<PreparedSource> {
    let file_size = fingerprint.file_size;
    let modified_ns = fingerprint.modified_ns;
    let existing = ledger.source_checkpoint(&source.path)?;

    if !options.full && is_unchanged(source, existing.as_ref(), file_size, modified_ns) {
        return Ok(PreparedSource::Unchanged);
    }

    let mut reset = options.full || source.compressed || existing.is_none();
    let (start_offset, start_line, mut state) = if reset {
        (0, 0, Value::Null)
    } else {
        let checkpoint = existing
            .as_ref()
            .expect("existing checkpoint checked above");
        if checkpoint.client != source.client
            || checkpoint.compressed != source.compressed
            || checkpoint.checkpoint_offset > file_size
            || requires_backfill(checkpoint, file_size)
            || !checkpoint_still_valid(&source.path, checkpoint)?
        {
            reset = true;
            (0, 0, Value::Null)
        } else {
            (
                checkpoint.checkpoint_offset,
                checkpoint.checkpoint_line,
                checkpoint.adapter_state.clone(),
            )
        }
    };

    let parsed = if source.compressed {
        parse_compressed(adapter, &source.path, options.since)?
    } else {
        parse_plain(
            adapter,
            &source.path,
            start_offset,
            start_line,
            &mut state,
            options.since,
        )?
    };
    if source.compressed {
        state = parsed.next_state.clone();
    }

    let parsed_checkpoint_offset = if source.compressed {
        file_size
    } else {
        parsed.checkpoint_offset
    };
    // A persisted --since scan is a filtered view, not a durable declaration
    // that older records no longer matter. Leave a zero checkpoint marker so
    // the next unrestricted scan rebuilds this source and backfills history.
    let checkpoint_deferred = options.since.is_some();
    let checkpoint_offset = if checkpoint_deferred {
        0
    } else {
        parsed_checkpoint_offset
    };
    let checkpoint_line = if checkpoint_deferred {
        0
    } else {
        parsed.checkpoint_line
    };
    let persisted_state = if checkpoint_deferred {
        Value::Null
    } else {
        state.clone()
    };
    let checkpoint_hash = hash_window_ending_at(&source.path, checkpoint_offset)?;
    let head_hash = hash_head(&source.path, checkpoint_offset)?;
    Ok(PreparedSource::Scanned(PreparedSourceUpdate {
        reset,
        file_size,
        modified_ns,
        checkpoint_offset,
        checkpoint_line,
        checkpoint_hash,
        head_hash,
        persisted_state,
        batch: parsed.batch,
    }))
}

struct ParsedSource {
    batch: ParseBatch,
    checkpoint_offset: u64,
    checkpoint_line: u64,
    next_state: Value,
}

fn parse_plain(
    adapter: &dyn SourceAdapter,
    path: &Path,
    start_offset: u64,
    start_line: u64,
    state: &mut Value,
    since: Option<DateTime<Utc>>,
) -> Result<ParsedSource> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;
    let mut reader = BufReader::new(file);
    let mut offset = start_offset;
    let mut line_number = start_line;
    let mut records = Vec::with_capacity(LINE_BATCH_SIZE);
    let mut observations = Vec::new();
    let mut warnings = Vec::new();
    let mut incomplete_tail = false;

    loop {
        let line_start = offset;
        let mut bytes = Vec::new();
        let read = reader.read_until(b'\n', &mut bytes)?;
        if read == 0 {
            break;
        }
        if !bytes.ends_with(b"\n") {
            incomplete_tail = true;
            break;
        }
        offset = offset.saturating_add(read as u64);
        line_number += 1;
        trim_newline(&mut bytes);
        records.push(LineRecord {
            line_number,
            byte_start: line_start,
            byte_end: offset,
            text: String::from_utf8_lossy(&bytes).into_owned(),
        });
        if records.len() >= LINE_BATCH_SIZE {
            consume_batch(
                adapter,
                path,
                &mut records,
                state,
                since,
                &mut observations,
                &mut warnings,
            )?;
        }
    }
    consume_batch(
        adapter,
        path,
        &mut records,
        state,
        since,
        &mut observations,
        &mut warnings,
    )?;
    if incomplete_tail {
        warnings.push(
            ScanWarning::new(
                "incomplete_tail",
                "final JSONL record is incomplete and will be retried on the next scan",
            )
            .at(format!("byte {offset}")),
        );
    }
    Ok(ParsedSource {
        batch: ParseBatch {
            observations,
            warnings,
            next_state: state.clone(),
        },
        checkpoint_offset: offset,
        checkpoint_line: line_number,
        next_state: state.clone(),
    })
}

fn parse_compressed(
    adapter: &dyn SourceAdapter,
    path: &Path,
    since: Option<DateTime<Utc>>,
) -> Result<ParsedSource> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("failed to open zstd source {}", path.display()))?;
    let mut reader = BufReader::new(decoder);
    let mut records = Vec::with_capacity(LINE_BATCH_SIZE);
    let mut observations = Vec::new();
    let mut warnings = Vec::new();
    let mut state = Value::Null;
    let mut offset = 0_u64;
    let mut line_number = 0_u64;
    loop {
        let line_start = offset;
        let mut bytes = Vec::new();
        let read = reader.read_until(b'\n', &mut bytes)?;
        if read == 0 {
            break;
        }
        offset = offset.saturating_add(read as u64);
        line_number += 1;
        trim_newline(&mut bytes);
        records.push(LineRecord {
            line_number,
            byte_start: line_start,
            byte_end: offset,
            text: String::from_utf8_lossy(&bytes).into_owned(),
        });
        if records.len() >= LINE_BATCH_SIZE {
            consume_batch(
                adapter,
                path,
                &mut records,
                &mut state,
                since,
                &mut observations,
                &mut warnings,
            )?;
        }
    }
    consume_batch(
        adapter,
        path,
        &mut records,
        &mut state,
        since,
        &mut observations,
        &mut warnings,
    )?;
    Ok(ParsedSource {
        batch: ParseBatch {
            observations,
            warnings,
            next_state: state.clone(),
        },
        checkpoint_offset: offset,
        checkpoint_line: line_number,
        next_state: state,
    })
}

fn consume_batch(
    adapter: &dyn SourceAdapter,
    path: &Path,
    records: &mut Vec<LineRecord>,
    state: &mut Value,
    since: Option<DateTime<Utc>>,
    observations: &mut Vec<UsageObservation>,
    warnings: &mut Vec<ScanWarning>,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let batch = adapter.parse_lines(path, records, Some(state))?;
    *state = batch.next_state;
    observations.extend(
        batch
            .observations
            .into_iter()
            .filter(|observation| since.is_none_or(|cutoff| observation.occurred_at >= cutoff)),
    );
    warnings.extend(batch.warnings);
    records.clear();
    Ok(())
}

fn is_unchanged(
    source: &SourceSpec,
    existing: Option<&SourceCheckpoint>,
    file_size: u64,
    modified_ns: i64,
) -> bool {
    existing.is_some_and(|checkpoint| {
        checkpoint.client == source.client
            && checkpoint.compressed == source.compressed
            && checkpoint.file_size == file_size
            && checkpoint.modified_ns == modified_ns
            && !requires_backfill(checkpoint, file_size)
    })
}

fn requires_backfill(checkpoint: &SourceCheckpoint, current_size: u64) -> bool {
    current_size > 0 && checkpoint.checkpoint_offset == 0 && checkpoint.checkpoint_line == 0
}

fn checkpoint_still_valid(path: &Path, checkpoint: &SourceCheckpoint) -> Result<bool> {
    if checkpoint.head_hash != hash_head(path, checkpoint.checkpoint_offset)? {
        return Ok(false);
    }
    Ok(checkpoint.checkpoint_hash == hash_window_ending_at(path, checkpoint.checkpoint_offset)?)
}

fn hash_head(path: &Path, checkpoint_offset: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let length = checkpoint_offset.min(CHECKPOINT_WINDOW) as usize;
    let mut buffer = vec![0_u8; length];
    file.read_exact(&mut buffer)?;
    Ok(hex::encode(Sha256::digest(&buffer)))
}

fn hash_window_ending_at(path: &Path, offset: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let start = offset.saturating_sub(CHECKPOINT_WINDOW);
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = vec![0_u8; (offset - start) as usize];
    file.read_exact(&mut buffer)?;
    Ok(hex::encode(Sha256::digest(&buffer)))
}

/// A bounded content fingerprint supplements size/mtime checks so fast
/// same-size rewrites are still detected without hashing whole transcripts.
fn source_fingerprint(path: &Path) -> Result<SourceFingerprint> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        anyhow::bail!("source is not a regular file");
    }
    let file_size = metadata.len();
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    hasher.update(file_size.to_le_bytes());

    let head_len = file_size.min(FINGERPRINT_WINDOW) as usize;
    let mut head = vec![0_u8; head_len];
    file.read_exact(&mut head)?;
    hasher.update(&head);

    if file_size > FINGERPRINT_WINDOW {
        let tail_start = file_size.saturating_sub(FINGERPRINT_WINDOW);
        file.seek(SeekFrom::Start(tail_start))?;
        let mut tail = vec![0_u8; (file_size - tail_start) as usize];
        file.read_exact(&mut tail)?;
        hasher.update(&tail);
    }

    Ok(SourceFingerprint {
        file_size,
        modified_ns: modified_ns(&metadata),
        sample_hash: hex::encode(hasher.finalize()),
    })
}

fn modified_ns(metadata: &Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
        .unwrap_or_default()
}

fn trim_newline(bytes: &mut Vec<u8>) {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::{NamedTempFile, tempdir};

    use crate::adapters::claude::ClaudeAdapter;

    fn claude_record(id: &str, output_tokens: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-07-10T12:00:00Z",
            "sessionId": "session",
            "message": {
                "id": id,
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 1,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": output_tokens
                }
            }
        })
        .to_string()
            + "\n"
    }

    struct AppendOnceAdapter {
        source: PathBuf,
        appended: AtomicBool,
    }

    impl SourceAdapter for AppendOnceAdapter {
        fn client(&self) -> Client {
            Client::ClaudeCode
        }

        fn display_name(&self) -> &'static str {
            "append-once test"
        }

        fn discover(&self, _config: &Config) -> Result<Vec<SourceSpec>> {
            Ok(vec![SourceSpec {
                path: self.source.clone(),
                client: Client::ClaudeCode,
                compressed: false,
            }])
        }

        fn parse_lines(
            &self,
            path: &Path,
            lines: &[LineRecord],
            previous_state: Option<&Value>,
        ) -> Result<ParseBatch> {
            if !self.appended.swap(true, Ordering::SeqCst) {
                OpenOptions::new()
                    .append(true)
                    .open(path)?
                    .write_all(claude_record("appended", 2).as_bytes())?;
            }
            ClaudeAdapter.parse_lines(path, lines, previous_state)
        }
    }

    struct AppendEarlierSourceAdapter {
        sources: Vec<PathBuf>,
        earlier_source: PathBuf,
        append_while_parsing: PathBuf,
        appended: AtomicBool,
    }

    impl SourceAdapter for AppendEarlierSourceAdapter {
        fn client(&self) -> Client {
            Client::ClaudeCode
        }

        fn display_name(&self) -> &'static str {
            "post-validation test"
        }

        fn discover(&self, _config: &Config) -> Result<Vec<SourceSpec>> {
            Ok(self
                .sources
                .iter()
                .map(|path| SourceSpec {
                    path: path.clone(),
                    client: Client::ClaudeCode,
                    compressed: false,
                })
                .collect())
        }

        fn parse_lines(
            &self,
            path: &Path,
            lines: &[LineRecord],
            previous_state: Option<&Value>,
        ) -> Result<ParseBatch> {
            if path == self.append_while_parsing && !self.appended.swap(true, Ordering::SeqCst) {
                OpenOptions::new()
                    .append(true)
                    .open(&self.earlier_source)?
                    .write_all(claude_record("late", 3).as_bytes())?;
            }
            ClaudeAdapter.parse_lines(path, lines, previous_state)
        }
    }

    #[test]
    fn checkpoint_hash_changes_when_prefix_changes() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        file.write_all(b"first\nsecond\n")?;
        let path = file.path().to_path_buf();
        let before = hash_window_ending_at(&path, 6)?;
        file.as_file_mut().seek(SeekFrom::Start(0))?;
        file.as_file_mut().write_all(b"other")?;
        file.as_file_mut().flush()?;
        let after = hash_window_ending_at(&path, 6)?;
        assert_ne!(before, after);
        Ok(())
    }

    #[test]
    fn head_hash_ignores_bytes_appended_after_small_checkpoint() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        file.write_all(b"first\n")?;
        file.as_file_mut().flush()?;
        let before = hash_head(file.path(), 6)?;

        file.write_all(b"second\n")?;
        file.as_file_mut().flush()?;
        let after = hash_head(file.path(), 6)?;

        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    fn append_during_parse_is_retried_and_snapshot_is_provisional() -> Result<()> {
        let dir = tempdir()?;
        let source = dir.path().join("active.jsonl");
        std::fs::write(&source, claude_record("initial", 1))?;
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(AppendOnceAdapter {
            source: source.clone(),
            appended: AtomicBool::new(false),
        })];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        let summary = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(summary.scanned_sources, 1);
        assert_eq!(summary.observations, 2);
        assert_eq!(summary.active_or_volatile_source_count, 1);
        assert!(summary.provisional);
        assert_eq!(ledger.canonical_events(None, None)?.len(), 2);

        let coverage = ledger.coverage_snapshot()?;
        assert_eq!(coverage.active_or_volatile_source_count, 1);
        assert!(coverage.provisional);
        assert_eq!(coverage.as_of, summary.as_of);
        Ok(())
    }

    #[test]
    fn post_scan_revalidation_detects_append_after_an_earlier_source() -> Result<()> {
        let dir = tempdir()?;
        let earlier = dir.path().join("a.jsonl");
        let later = dir.path().join("b.jsonl");
        std::fs::write(&earlier, claude_record("a", 1))?;
        std::fs::write(&later, claude_record("b", 2))?;
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(AppendEarlierSourceAdapter {
            sources: vec![earlier.clone(), later.clone()],
            earlier_source: earlier,
            append_while_parsing: later,
            appended: AtomicBool::new(false),
        })];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        let first = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(first.active_or_volatile_source_count, 1);
        assert!(first.provisional);
        assert_eq!(ledger.canonical_events(None, None)?.len(), 2);

        let second = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(second.active_or_volatile_source_count, 0);
        assert!(!second.provisional);
        assert_eq!(ledger.canonical_events(None, None)?.len(), 3);
        Ok(())
    }

    #[test]
    fn persisted_since_scan_defers_checkpoint_and_later_backfills() -> Result<()> {
        let dir = tempdir()?;
        let claude_root = dir.path().join("claude");
        let project = claude_root.join("projects").join("project");
        std::fs::create_dir_all(&project)?;
        let source = project.join("session.jsonl");
        std::fs::write(
            &source,
            concat!(
                "{\"type\":\"assistant\",\"timestamp\":\"2026-07-09T12:00:00Z\",\"sessionId\":\"session\",\"message\":{\"id\":\"old\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":1,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":1}}}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-07-10T12:00:00Z\",\"sessionId\":\"session\",\"message\":{\"id\":\"new\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":2}}}\n"
            ),
        )?;
        let config = Config {
            claude_root: Some(claude_root),
            ..Default::default()
        };
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ClaudeAdapter)];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        let limited = ScanOptions {
            since: Some(Utc.with_ymd_and_hms(2026, 7, 10, 0, 0, 0).unwrap()),
            ..Default::default()
        };
        scan(&mut ledger, &config, &adapters, &limited)?;
        let limited_events = ledger.canonical_events(None, None)?;
        assert_eq!(limited_events.len(), 1);
        assert_eq!(limited_events[0].usage.output_tokens_total, 2);
        let deferred = ledger
            .source_checkpoint(&source)?
            .context("limited scan did not persist its source")?;
        assert_eq!(deferred.checkpoint_offset, 0);
        assert_eq!(deferred.checkpoint_line, 0);
        assert!(deferred.adapter_state.is_null());

        scan(&mut ledger, &config, &adapters, &ScanOptions::default())?;
        let complete_events = ledger.canonical_events(None, None)?;
        assert_eq!(complete_events.len(), 2);
        assert_eq!(
            complete_events
                .iter()
                .map(|event| event.usage.output_tokens_total)
                .sum::<u64>(),
            3
        );
        let complete = ledger
            .source_checkpoint(&source)?
            .context("backfill scan lost its source")?;
        assert_eq!(complete.checkpoint_offset, std::fs::metadata(source)?.len());
        assert_eq!(complete.checkpoint_line, 2);
        Ok(())
    }
}
