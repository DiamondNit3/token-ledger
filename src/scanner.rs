use std::collections::{HashSet, VecDeque};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::adapters::{DiscoveryRequest, SourceAdapter};
use crate::config::Config;
use crate::db::{Ledger, SourceCheckpoint, SourceUpdate};
use crate::model::{
    Client, LineRecord, ParseBatch, ScanWarning, SourceSpec, UsageObservation, stable_id,
};

const LINE_BATCH_SIZE: usize = 512;
const LINE_BATCH_BYTES: usize = 4 * 1024 * 1024;
const CHECKPOINT_WINDOW: u64 = 4096;
const EXACT_FINGERPRINT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOURCES_PER_ADAPTER_SCAN: usize = 4_096;
const SOURCE_STABILITY_ATTEMPTS: usize = 3;
const MAX_SOURCE_FILE_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct ParseLimits {
    max_bytes: u64,
    max_line_bytes: usize,
    max_records: u64,
    max_observations: usize,
    max_warnings: usize,
}

#[derive(Debug, Clone, Copy)]
struct ScanLimits {
    max_sources_per_adapter: usize,
    max_discovery_entries_per_adapter: usize,
    max_total_sources: usize,
    max_total_work_bytes: u64,
    max_total_records: u64,
    max_total_observations: usize,
    max_total_warnings: usize,
    max_source_file_bytes: u64,
    plain: ParseLimits,
    compressed: ParseLimits,
    zstd_window_log_max: u32,
}

const DEFAULT_SCAN_LIMITS: ScanLimits = ScanLimits {
    max_sources_per_adapter: MAX_SOURCES_PER_ADAPTER_SCAN,
    max_discovery_entries_per_adapter: crate::adapters::MAX_DISCOVERY_ENTRIES,
    max_total_sources: MAX_SOURCES_PER_ADAPTER_SCAN * 2,
    max_total_work_bytes: 2 * 1024 * 1024 * 1024,
    max_total_records: 5_000_000,
    max_total_observations: 2_000_000,
    max_total_warnings: 100_000,
    // Keep the largest admitted source within the conservative 2 GiB
    // aggregate-work estimate. A source above this bound is an explicit hard
    // stop rather than a permanently starved item in the rotating scheduler.
    max_source_file_bytes: MAX_SOURCE_FILE_BYTES,
    plain: ParseLimits {
        max_bytes: 64 * 1024 * 1024,
        max_line_bytes: 8 * 1024 * 1024,
        max_records: 1_000_000,
        max_observations: 1_000_000,
        max_warnings: 10_000,
    },
    compressed: ParseLimits {
        max_bytes: 128 * 1024 * 1024,
        max_line_bytes: 8 * 1024 * 1024,
        max_records: 1_000_000,
        max_observations: 1_000_000,
        max_warnings: 10_000,
    },
    // Bound decoder history to 128 MiB even if an input frame advertises a
    // much larger back-reference window.
    zstd_window_log_max: 27,
};

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
    /// Source candidates observed within this invocation's bounded discovery
    /// passes. This is a lower bound when `coverage_limited` is true.
    pub discovered_sources: u64,
    pub scanned_sources: u64,
    pub unchanged_sources: u64,
    pub observations: u64,
    pub warnings: u64,
    pub reset_sources: u64,
    pub as_of: Option<DateTime<Utc>>,
    pub active_or_volatile_source_count: u64,
    pub incomplete_source_count: u64,
    pub coverage_limited: bool,
    pub provisional: bool,
    pub dry_run: bool,
}

pub fn scan(
    ledger: &mut Ledger,
    config: &Config,
    adapters: &[Box<dyn SourceAdapter>],
    options: &ScanOptions,
) -> Result<ScanSummary> {
    scan_with_limits(ledger, config, adapters, options, DEFAULT_SCAN_LIMITS)
}

fn scan_with_limits(
    ledger: &mut Ledger,
    config: &Config,
    adapters: &[Box<dyn SourceAdapter>],
    options: &ScanOptions,
    limits: ScanLimits,
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
    let mut stable_sources = Vec::<(PathBuf, PathBuf, SourceFingerprint)>::new();
    let mut active_or_volatile_sources = HashSet::<PathBuf>::new();
    let mut incomplete_sources = HashSet::<PathBuf>::new();
    let client_scope_limited = !options.clients.is_empty()
        && Client::ALL
            .iter()
            .any(|client| !options.clients.contains(client));
    let mut coverage_incomplete = options.since.is_some() || client_scope_limited;
    let mut remaining_sources = limits.max_total_sources;
    let mut remaining_work_bytes = limits.max_total_work_bytes;
    let mut remaining_records = limits.max_total_records;
    let mut remaining_observations = limits.max_total_observations;
    let mut remaining_warnings = limits.max_total_warnings;
    let mut scan_resource_warning_recorded = false;
    let mut revalidation_boundary = None;
    let rotation_seed = scan_run_id
        .and_then(|run_id| usize::try_from(run_id.saturating_sub(1)).ok())
        .unwrap_or(0);

    let result = (|| -> Result<()> {
        if options.since.is_some() {
            let warning = ScanWarning::new(
                "scan_limited",
                "scan used a caller-supplied time cutoff; history before that cutoff is outside this snapshot",
            );
            persist_scan_warning(
                ledger,
                scan_run_id,
                None,
                &warning,
                &mut summary,
                &mut remaining_warnings,
            )?;
        }
        if client_scope_limited {
            let warning = ScanWarning::new(
                "scan_client_scope_limited",
                "scan excluded one or more supported clients; the global snapshot remains provisional",
            );
            persist_scan_warning(
                ledger,
                scan_run_id,
                None,
                &warning,
                &mut summary,
                &mut remaining_warnings,
            )?;
        }
        let mut selected_adapters = adapters
            .iter()
            .filter(|adapter| options.includes(adapter.client()))
            .collect::<Vec<_>>();
        if !selected_adapters.is_empty() {
            let adapter_count = selected_adapters.len();
            selected_adapters.rotate_left(rotation_seed % adapter_count);
        }
        for adapter in selected_adapters {
            let discovery_request = DiscoveryRequest::scanner(
                rotation_seed,
                limits.max_sources_per_adapter,
                limits.max_discovery_entries_per_adapter,
            );
            let discovery = match adapter.discover_bounded(config, discovery_request) {
                Ok(discovery) => discovery,
                Err(_error) => {
                    coverage_incomplete = true;
                    let warning = ScanWarning::new(
                        "discovery_failed",
                        format!(
                            "{} source discovery failed; verify that its configured root is readable",
                            adapter.display_name()
                        ),
                    );
                    persist_scan_warning(
                        ledger,
                        scan_run_id,
                        None,
                        &warning,
                        &mut summary,
                        &mut remaining_warnings,
                    )?;
                    continue;
                }
            };
            summary.discovered_sources += discovery.observed_source_count as u64;
            if discovery.source_limit_reached {
                coverage_incomplete = true;
                let warning = ScanWarning::new(
                    "discovery_source_limit",
                    format!(
                        "{} source discovery exceeded its bounded candidate window; a safe subset of observed candidates was retained",
                        adapter.display_name()
                    ),
                );
                persist_scan_warning(
                    ledger,
                    scan_run_id,
                    None,
                    &warning,
                    &mut summary,
                    &mut remaining_warnings,
                )?;
            }
            if discovery.entry_limit_reached {
                coverage_incomplete = true;
                let warning = ScanWarning::new(
                    "discovery_entry_limit",
                    format!(
                        "{} source discovery reached its bounded traversal limit; a safe partial result was retained and not all entries were examined",
                        adapter.display_name()
                    ),
                );
                persist_scan_warning(
                    ledger,
                    scan_run_id,
                    None,
                    &warning,
                    &mut summary,
                    &mut remaining_warnings,
                )?;
            }
            if discovery.io_incomplete {
                coverage_incomplete = true;
                let warning = ScanWarning::new(
                    "discovery_partial",
                    format!(
                        "{} source discovery encountered an unreadable nested entry; safe bounded candidates were retained",
                        adapter.display_name()
                    ),
                );
                persist_scan_warning(
                    ledger,
                    scan_run_id,
                    None,
                    &warning,
                    &mut summary,
                    &mut remaining_warnings,
                )?;
            }
            let mut sources = discovery.sources;
            sources.sort_by(|left, right| left.path.cmp(&right.path));
            sources.dedup_by(|left, right| left.path == right.path);
            if !sources.is_empty() {
                let rotation = rotating_window_start(
                    rotation_seed,
                    limits.max_sources_per_adapter,
                    sources.len(),
                );
                sources.rotate_left(rotation);
            }
            if sources.len() > limits.max_sources_per_adapter {
                coverage_incomplete = true;
                let warning = ScanWarning::new(
                    "source_discovery_limit",
                    format!(
                        "{} source discovery reached its bounded result limit; remaining sources were not scanned",
                        adapter.display_name()
                    ),
                );
                persist_scan_warning(
                    ledger,
                    scan_run_id,
                    None,
                    &warning,
                    &mut summary,
                    &mut remaining_warnings,
                )?;
                sources.truncate(limits.max_sources_per_adapter);
            }
            sources = schedule_sources(ledger, sources, rotation_seed, limits)?;

            for source in sources {
                let estimated_work = estimate_source_work(&source, limits)
                    .unwrap_or(1024 * 1024)
                    .max(1);
                if remaining_sources == 0
                    || estimated_work > remaining_work_bytes
                    || remaining_records == 0
                    || remaining_observations == 0
                    || remaining_warnings == 0
                {
                    coverage_incomplete = true;
                    if !scan_resource_warning_recorded {
                        let warning = ScanWarning::new(
                            "scan_resource_limit",
                            "scan reached an aggregate resource bound; remaining sources were not scanned in this snapshot",
                        );
                        persist_scan_warning(
                            ledger,
                            scan_run_id,
                            None,
                            &warning,
                            &mut summary,
                            &mut remaining_warnings,
                        )?;
                        scan_resource_warning_recorded = true;
                    }
                    continue;
                }
                remaining_sources -= 1;
                remaining_work_bytes -= estimated_work;
                let mut source_limits = limits;
                source_limits.plain.max_records =
                    source_limits.plain.max_records.min(remaining_records);
                source_limits.compressed.max_records =
                    source_limits.compressed.max_records.min(remaining_records);
                source_limits.plain.max_observations = source_limits
                    .plain
                    .max_observations
                    .min(remaining_observations);
                source_limits.compressed.max_observations = source_limits
                    .compressed
                    .max_observations
                    .min(remaining_observations);
                // Leave room for the scanner's own bounded-stop warning if
                // the adapter output reaches the per-invocation warning cap.
                let parse_warning_budget = remaining_warnings.saturating_sub(1);
                source_limits.plain.max_warnings =
                    source_limits.plain.max_warnings.min(parse_warning_budget);
                source_limits.compressed.max_warnings = source_limits
                    .compressed
                    .max_warnings
                    .min(parse_warning_budget);
                let aggregate_count_clamped = if source.compressed {
                    source_limits.compressed.max_records < limits.compressed.max_records
                        || source_limits.compressed.max_observations
                            < limits.compressed.max_observations
                        || source_limits.compressed.max_warnings < limits.compressed.max_warnings
                } else {
                    source_limits.plain.max_records < limits.plain.max_records
                        || source_limits.plain.max_observations < limits.plain.max_observations
                        || source_limits.plain.max_warnings < limits.plain.max_warnings
                };
                if let Some(run_id) = scan_run_id {
                    ledger.heartbeat_scan(run_id)?;
                }
                let mut work_reservation = SourceWorkReservation {
                    reserved_bytes: estimated_work,
                    remaining_bytes: &mut remaining_work_bytes,
                };
                match scan_one(
                    ledger,
                    adapter.as_ref(),
                    &source,
                    options,
                    scan_run_id,
                    source_limits,
                    &mut work_reservation,
                ) {
                    Ok(SourceOutcome::Unchanged {
                        fingerprint,
                        active_during_scan,
                    }) => {
                        summary.unchanged_sources += 1;
                        if active_during_scan {
                            active_or_volatile_sources.insert(source.path.clone());
                        }
                        if !fingerprint.exhaustive {
                            incomplete_sources.insert(source.path.clone());
                        }
                        if !fingerprint.exhaustive {
                            let warning = sampled_verification_warning(adapter.as_ref());
                            persist_scan_warning(
                                ledger,
                                scan_run_id,
                                Some(&source),
                                &warning,
                                &mut summary,
                                &mut remaining_warnings,
                            )?;
                        }
                        stable_sources.push((
                            source.path.clone(),
                            source.trusted_root.clone(),
                            fingerprint,
                        ));
                    }
                    Ok(SourceOutcome::Scanned {
                        records,
                        observations,
                        warnings,
                        reset,
                        fingerprint,
                        active_during_scan,
                        complete,
                        resource_limited,
                    }) => {
                        remaining_records = remaining_records.saturating_sub(records);
                        remaining_observations = remaining_observations
                            .saturating_sub(usize::try_from(observations).unwrap_or(usize::MAX));
                        remaining_warnings = remaining_warnings
                            .saturating_sub(usize::try_from(warnings).unwrap_or(usize::MAX));
                        if resource_limited
                            && (aggregate_count_clamped
                                || remaining_records == 0
                                || remaining_observations == 0
                                || remaining_warnings == 0)
                        {
                            coverage_incomplete = true;
                        }
                        summary.scanned_sources += 1;
                        summary.observations += observations;
                        summary.warnings += warnings;
                        summary.reset_sources += u64::from(reset);
                        if active_during_scan {
                            active_or_volatile_sources.insert(source.path.clone());
                        }
                        if !complete || !fingerprint.exhaustive {
                            incomplete_sources.insert(source.path.clone());
                        }
                        if !fingerprint.exhaustive {
                            let warning = sampled_verification_warning(adapter.as_ref());
                            persist_scan_warning(
                                ledger,
                                scan_run_id,
                                Some(&source),
                                &warning,
                                &mut summary,
                                &mut remaining_warnings,
                            )?;
                        }
                        stable_sources.push((
                            source.path.clone(),
                            source.trusted_root.clone(),
                            fingerprint,
                        ));
                    }
                    Ok(SourceOutcome::Limited(warning)) => {
                        incomplete_sources.insert(source.path.clone());
                        persist_scan_warning(
                            ledger,
                            scan_run_id,
                            Some(&source),
                            &warning,
                            &mut summary,
                            &mut remaining_warnings,
                        )?;
                    }
                    Ok(SourceOutcome::Volatile) => {
                        active_or_volatile_sources.insert(source.path.clone());
                        let warning = ScanWarning::new(
                            "source_volatile",
                            format!(
                                "{} source remained active during bounded stability retries; it will be retried on the next scan",
                                adapter.display_name()
                            ),
                        );
                        persist_scan_warning(
                            ledger,
                            scan_run_id,
                            Some(&source),
                            &warning,
                            &mut summary,
                            &mut remaining_warnings,
                        )?;
                    }
                    Err(_error) => {
                        incomplete_sources.insert(source.path.clone());
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
                        persist_scan_warning(
                            ledger,
                            scan_run_id,
                            Some(&source),
                            &warning,
                            &mut summary,
                            &mut remaining_warnings,
                        )?;
                    }
                }
            }
        }

        // A source can become active after its individual parse succeeds.
        // Capture the common boundary before revalidation so every source's
        // original and final fingerprints bracket the reported `as_of`.
        revalidation_boundary = Some(Utc::now());
        for (index, (path, trusted_root, fingerprint)) in stable_sources.iter().enumerate() {
            if index % 32 == 0
                && let Some(run_id) = scan_run_id
            {
                ledger.heartbeat_scan(run_id)?;
            }
            if source_fingerprint_within(path, trusted_root).as_ref().ok() != Some(fingerprint) {
                active_or_volatile_sources.insert(path.clone());
            }
        }
        Ok(())
    })();

    // A failure before final revalidation has no stable boundary; its failed
    // snapshot still receives a completion timestamp for diagnostics.
    let as_of = revalidation_boundary.unwrap_or_else(Utc::now);
    summary.as_of = Some(as_of);
    incomplete_sources.retain(|path| !active_or_volatile_sources.contains(path));
    summary.active_or_volatile_source_count = active_or_volatile_sources.len() as u64;
    summary.incomplete_source_count = incomplete_sources.len() as u64;
    summary.coverage_limited = coverage_incomplete;
    summary.provisional = result.is_err()
        || summary.coverage_limited
        || summary.active_or_volatile_source_count > 0
        || summary.incomplete_source_count > 0;
    if let Some(run_id) = scan_run_id {
        let status = if result.is_err() {
            "failed"
        } else if summary.provisional {
            "partial"
        } else {
            "ok"
        };
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

fn persist_scan_warning(
    ledger: &Ledger,
    scan_run_id: Option<i64>,
    source: Option<&SourceSpec>,
    warning: &ScanWarning,
    summary: &mut ScanSummary,
    remaining_warnings: &mut usize,
) -> Result<bool> {
    if *remaining_warnings == 0 {
        return Ok(false);
    }
    *remaining_warnings -= 1;
    summary.warnings += 1;
    let Some(run_id) = scan_run_id else {
        return Ok(true);
    };
    let source_id = source
        .map(|source| ledger.ensure_source(source.client, &source.path, source.compressed))
        .transpose()?;
    ledger.record_scan_warning(run_id, source_id, warning)?;
    Ok(true)
}

fn sampled_verification_warning(adapter: &dyn SourceAdapter) -> ScanWarning {
    ScanWarning::new(
        "source_verification_sampled",
        format!(
            "{} source exceeded the exact fingerprint bound; sampled verification cannot establish complete coverage",
            adapter.display_name()
        ),
    )
}

#[derive(Debug, Clone, Copy)]
enum SourceScheduleClass {
    Unseen = 0,
    Deferred = 1,
    Known = 2,
}

impl SourceScheduleClass {
    const COUNT: usize = 3;

    fn index(self) -> usize {
        self as usize
    }
}

/// Orders one bounded discovery window without persisting any newly discovered
/// paths. Unseen and deferred sources receive most admission slots, while a
/// smaller known-source share preserves rewrite revalidation. Each class uses
/// a budget-derived coarse rotation so a work-limited scan advances by roughly
/// the number of sources it can admit instead of shifting by one path per run.
fn schedule_sources(
    ledger: &Ledger,
    mut sources: Vec<SourceSpec>,
    rotation_seed: usize,
    limits: ScanLimits,
) -> Result<Vec<SourceSpec>> {
    if sources.len() <= 1 {
        return Ok(sources);
    }
    sources.sort_by(|left, right| left.path.cmp(&right.path));

    let admission_window = estimated_admission_window(&sources, limits);
    let mut buckets: [Vec<SourceSpec>; SourceScheduleClass::COUNT] =
        std::array::from_fn(|_| Vec::new());
    for source in sources {
        let class = source_schedule_class(ledger, &source)?;
        buckets[class.index()].push(source);
    }

    // Two unseen slots, one deferred slot, and one known-source validation
    // slot. Rotating the pattern prevents a one-source invocation from
    // permanently starving any non-empty class.
    const PATTERN: [SourceScheduleClass; 4] = [
        SourceScheduleClass::Unseen,
        SourceScheduleClass::Deferred,
        SourceScheduleClass::Unseen,
        SourceScheduleClass::Known,
    ];
    let lengths = [buckets[0].len(), buckets[1].len(), buckets[2].len()];
    let mut simulated_remaining = lengths;
    let mut quotas = [0_usize; SourceScheduleClass::COUNT];
    let mut admitted = 0_usize;
    let mut cursor = 0_usize;
    while admitted < admission_window && simulated_remaining.iter().any(|remaining| *remaining > 0)
    {
        let class =
            PATTERN[(rotation_seed % PATTERN.len() + cursor % PATTERN.len()) % PATTERN.len()];
        cursor += 1;
        let index = class.index();
        if simulated_remaining[index] == 0 {
            continue;
        }
        simulated_remaining[index] -= 1;
        quotas[index] += 1;
        admitted += 1;
    }

    for index in 0..SourceScheduleClass::COUNT {
        let source_count = buckets[index].len();
        if source_count > 1 {
            let rotation = rotating_window_start(rotation_seed, quotas[index].max(1), source_count);
            buckets[index].rotate_left(rotation);
        }
    }

    let mut queues: [VecDeque<SourceSpec>; SourceScheduleClass::COUNT] =
        buckets.map(VecDeque::from);
    let total_sources = lengths.into_iter().sum();
    let mut scheduled = Vec::with_capacity(total_sources);
    cursor = 0;
    while scheduled.len() < total_sources {
        let class =
            PATTERN[(rotation_seed % PATTERN.len() + cursor % PATTERN.len()) % PATTERN.len()];
        cursor += 1;
        if let Some(source) = queues[class.index()].pop_front() {
            scheduled.push(source);
        }
    }
    Ok(scheduled)
}

fn source_schedule_class(ledger: &Ledger, source: &SourceSpec) -> Result<SourceScheduleClass> {
    let Some(checkpoint) = ledger.source_checkpoint(&source.path)? else {
        return Ok(SourceScheduleClass::Unseen);
    };
    if checkpoint.client != source.client
        || checkpoint.compressed != source.compressed
        || checkpoint_is_partial(&checkpoint)
    {
        return Ok(SourceScheduleClass::Deferred);
    }

    let Ok(metadata) = std::fs::metadata(&source.path) else {
        return Ok(SourceScheduleClass::Deferred);
    };
    if !metadata.is_file()
        || metadata.len() != checkpoint.file_size
        || modified_ns(&metadata) != checkpoint.modified_ns
        || metadata.len() > EXACT_FINGERPRINT_BYTES
    {
        return Ok(SourceScheduleClass::Deferred);
    }
    Ok(SourceScheduleClass::Known)
}

fn estimated_admission_window(sources: &[SourceSpec], limits: ScanLimits) -> usize {
    if sources.is_empty() {
        return 0;
    }
    let total_estimated_work = sources.iter().fold(0_u128, |total, source| {
        total.saturating_add(u128::from(
            estimate_source_work(source, limits)
                .unwrap_or(1024 * 1024)
                .max(1),
        ))
    });
    let average_work = total_estimated_work.div_ceil(sources.len() as u128).max(1);
    let admitted_by_work = (u128::from(limits.max_total_work_bytes) / average_work).max(1);
    usize::try_from(admitted_by_work)
        .unwrap_or(usize::MAX)
        .min(limits.max_total_sources.max(1))
        .min(limits.max_sources_per_adapter.max(1))
        .min(sources.len())
        .max(1)
}

fn rotating_window_start(seed: usize, window: usize, source_count: usize) -> usize {
    if source_count == 0 {
        return 0;
    }
    let window = window.max(1);
    let windows_per_cycle = source_count.div_ceil(window);
    let cycle = seed / windows_per_cycle;
    let window_index = seed % windows_per_cycle;
    // Visit every coarse window, then shift the base by one for the next
    // cycle. Even if the aggregate work budget admits only the first source
    // in each window, every path eventually becomes that first source.
    let start = ((cycle % window) as u128)
        .saturating_add((window_index as u128).saturating_mul(window as u128))
        .rem_euclid(source_count as u128);
    usize::try_from(start).expect("rotation remainder fits usize")
}

/// Conservative upper bound for content hashing, bounded stability retries,
/// parsing, and decompression. Reserving this before opening a source keeps one
/// invocation's aggregate work finite even when discovery returns many files.
fn estimate_source_work(source: &SourceSpec, limits: ScanLimits) -> Result<u64> {
    let file = open_source_file_within(&source.path, &source.trusted_root)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        anyhow::bail!("source is not a regular file");
    }
    Ok(estimate_source_work_for_size(
        metadata.len(),
        source.compressed,
        limits,
    ))
}

fn estimate_source_work_for_size(file_size: u64, compressed: bool, limits: ScanLimits) -> u64 {
    if file_size > limits.max_source_file_bytes {
        return 1024 * 1024;
    }

    let parse_bytes = if compressed {
        limits.compressed.max_bytes
    } else {
        file_size.min(limits.plain.max_bytes)
    };
    let estimate = if file_size <= EXACT_FINGERPRINT_BYTES {
        file_size
            .saturating_mul(32)
            .saturating_add(parse_bytes.saturating_mul(3))
            .saturating_add(1024 * 1024)
    } else {
        let compressed_input = if compressed { file_size } else { 0 };
        file_size
            .saturating_mul(8)
            .saturating_add(parse_bytes.saturating_mul(3))
            .saturating_add(compressed_input.saturating_mul(3))
            .saturating_add(1024 * 1024)
    };
    estimate.max(1)
}

enum SourceOutcome {
    Unchanged {
        fingerprint: SourceFingerprint,
        active_during_scan: bool,
    },
    Scanned {
        records: u64,
        observations: u64,
        warnings: u64,
        reset: bool,
        fingerprint: SourceFingerprint,
        active_during_scan: bool,
        complete: bool,
        resource_limited: bool,
    },
    Limited(ScanWarning),
    Volatile,
}

struct SourceWorkReservation<'a> {
    reserved_bytes: u64,
    remaining_bytes: &'a mut u64,
}

impl SourceWorkReservation<'_> {
    fn admit(&mut self, opened_work: u64) -> bool {
        if opened_work <= self.reserved_bytes {
            return true;
        }
        let additional = opened_work - self.reserved_bytes;
        if additional > *self.remaining_bytes {
            return false;
        }
        *self.remaining_bytes -= additional;
        self.reserved_bytes = opened_work;
        true
    }

    fn admit_attempt(&mut self, opened_work: u64, attempt: usize) -> bool {
        let cumulative = opened_work.saturating_mul((attempt as u64).saturating_add(1));
        self.admit(cumulative)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceFingerprint {
    file_size: u64,
    modified_ns: i64,
    sample_hash: String,
    exhaustive: bool,
}

enum PreparedSource {
    Unchanged,
    Limited(ScanWarning),
    Scanned(Box<PreparedSourceUpdate>),
}

struct PreparedSourceUpdate {
    reset: bool,
    complete: bool,
    records: u64,
    file_size: u64,
    modified_ns: i64,
    checkpoint_offset: u64,
    checkpoint_line: u64,
    checkpoint_hash: String,
    head_hash: String,
    persisted_state: Value,
    content_hash: String,
    batch: ParseBatch,
}

fn scan_one(
    ledger: &mut Ledger,
    adapter: &dyn SourceAdapter,
    source: &SourceSpec,
    options: &ScanOptions,
    scan_run_id: Option<i64>,
    limits: ScanLimits,
    work_reservation: &mut SourceWorkReservation<'_>,
) -> Result<SourceOutcome> {
    let mut active_during_scan = false;
    for attempt in 0..SOURCE_STABILITY_ATTEMPTS {
        // Open without following links, then use this verified handle for the
        // complete fingerprint, checkpoint validation, and parse. The work
        // reservation is revalidated from this handle so a pathname swap
        // cannot turn a small scheduled source into unmetered work.
        let file = open_source_file_within(&source.path, &source.trusted_root)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            anyhow::bail!("source is not a regular file");
        }
        if metadata.len() > limits.max_source_file_bytes {
            return Ok(SourceOutcome::Limited(ScanWarning::new(
                "source_file_limit",
                "source exceeded the bounded file-size limit and was not read",
            )));
        }
        let opened_work = estimate_source_work_for_size(metadata.len(), source.compressed, limits);
        if !work_reservation.admit_attempt(opened_work, attempt) {
            return Ok(SourceOutcome::Limited(ScanWarning::new(
                "scan_resource_limit",
                "source changed or required another stability attempt that exceeded the remaining aggregate work budget; it was not read again",
            )));
        }
        let (snapshot, before) =
            match snapshot_source(&file, &metadata, limits.max_source_file_bytes) {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    active_during_scan = true;
                    if attempt + 1 == SOURCE_STABILITY_ATTEMPTS {
                        return Ok(SourceOutcome::Volatile);
                    }
                    continue;
                }
            };
        let prepared = prepare_source(ledger, adapter, source, options, &snapshot, &before, limits);
        let after = source_fingerprint_within(&source.path, &source.trusted_root);

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
            PreparedSource::Limited(warning) => Ok(SourceOutcome::Limited(warning)),
            PreparedSource::Scanned(prepared) => {
                let observation_count = prepared.batch.observations.len() as u64;
                let warning_count = prepared.batch.warnings.len() as u64;
                let resource_limited = prepared.batch.warnings.iter().any(|warning| {
                    matches!(
                        warning.code.as_str(),
                        "source_resource_limit" | "compressed_archive_unsupported"
                    )
                });
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
                    ledger.update_source_content_hash(source_id, &prepared.content_hash)?;
                }
                Ok(SourceOutcome::Scanned {
                    records: prepared.records,
                    observations: observation_count,
                    warnings: warning_count,
                    reset: prepared.reset,
                    fingerprint,
                    active_during_scan,
                    complete: prepared.complete,
                    resource_limited,
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
    file: &File,
    fingerprint: &SourceFingerprint,
    limits: ScanLimits,
) -> Result<PreparedSource> {
    let file_size = fingerprint.file_size;
    let modified_ns = fingerprint.modified_ns;
    let existing = ledger.source_checkpoint(&source.path)?;

    if source.compressed
        && !options.full
        && existing.as_ref().is_some_and(|checkpoint| {
            checkpoint.file_size == file_size
                && checkpoint.modified_ns == modified_ns
                && checkpoint
                    .adapter_state
                    .get("compressed_archive_unsupported")
                    .and_then(Value::as_bool)
                    == Some(true)
                && checkpoint
                    .adapter_state
                    .get("compressed_source_hash")
                    .and_then(Value::as_str)
                    == Some(fingerprint.sample_hash.as_str())
        })
    {
        return Ok(PreparedSource::Limited(compressed_archive_limit_warning()));
    }

    if !options.full
        && is_unchanged(
            source,
            existing.as_ref(),
            file_size,
            modified_ns,
            &fingerprint.sample_hash,
        )
        && checkpoint_still_valid_file(
            file,
            existing
                .as_ref()
                .expect("unchanged metadata requires a checkpoint"),
        )?
    {
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
            || (file_size <= checkpoint.file_size
                && checkpoint.content_hash != fingerprint.sample_hash)
            || requires_backfill(checkpoint, file_size)
            || (file_size <= checkpoint.file_size && modified_ns != checkpoint.modified_ns)
            || !checkpoint_still_valid_file(file, checkpoint)?
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
        parse_compressed(
            adapter,
            file.try_clone()?,
            &source.path,
            options.since,
            limits.compressed,
            limits.zstd_window_log_max,
        )?
    } else {
        parse_plain(
            adapter,
            file.try_clone()?,
            &source.path,
            ParseStart {
                offset: start_offset,
                line: start_line,
            },
            &mut state,
            options.since,
            limits.plain,
        )?
    };
    if source.compressed {
        state = parsed.next_state.clone();
        if !parsed.complete {
            // Compressed streams are not seekable using the durable source
            // checkpoint. Every incomplete result is therefore all-or-nothing,
            // including oversized records, truncated tails, adapter-level
            // gaps, and count/byte bounds. Never replace accounting with a
            // prefix that cannot advance on a later scan.
            state = serde_json::json!({
                "compressed_archive_unsupported": true,
                "compressed_source_hash": fingerprint.sample_hash,
            });
            let warning = compressed_archive_limit_warning();
            return Ok(PreparedSource::Scanned(Box::new(PreparedSourceUpdate {
                reset: true,
                complete: false,
                records: parsed.records,
                file_size,
                modified_ns,
                checkpoint_offset: 0,
                checkpoint_line: 0,
                checkpoint_hash: hash_window_ending_at_file(file, 0)?,
                head_hash: hash_head_file(file, 0)?,
                persisted_state: state,
                content_hash: fingerprint.sample_hash.clone(),
                batch: ParseBatch {
                    observations: Vec::new(),
                    warnings: vec![warning],
                    next_state: Value::Null,
                    incomplete: true,
                },
            })));
        }
    }

    let parsed_checkpoint_offset = if source.compressed {
        if parsed.complete { file_size } else { 0 }
    } else {
        parsed.checkpoint_offset
    };
    // A persisted --since scan is a filtered view, not a durable declaration
    // that older records no longer matter. Leave a zero checkpoint marker so
    // the next unrestricted scan rebuilds this source and backfills history.
    let checkpoint_deferred = options.since.is_some() || parsed.force_rebuild;
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
    let checkpoint_hash = hash_window_ending_at_file(file, checkpoint_offset)?;
    let head_hash = hash_head_file(file, checkpoint_offset)?;
    Ok(PreparedSource::Scanned(Box::new(PreparedSourceUpdate {
        reset,
        complete: options.since.is_none() && parsed.complete,
        records: parsed.records,
        file_size,
        modified_ns,
        checkpoint_offset,
        checkpoint_line,
        checkpoint_hash,
        head_hash,
        persisted_state,
        content_hash: fingerprint.sample_hash.clone(),
        batch: parsed.batch,
    })))
}

struct ParsedSource {
    batch: ParseBatch,
    checkpoint_offset: u64,
    checkpoint_line: u64,
    next_state: Value,
    complete: bool,
    force_rebuild: bool,
    records: u64,
}

struct ParseContext<'a> {
    adapter: &'a dyn SourceAdapter,
    path: &'a Path,
    since: Option<DateTime<Utc>>,
    limits: ParseLimits,
}

#[derive(Clone, Copy)]
struct ParseStart {
    offset: u64,
    line: u64,
}

fn parse_plain(
    adapter: &dyn SourceAdapter,
    mut file: File,
    path: &Path,
    start: ParseStart,
    state: &mut Value,
    since: Option<DateTime<Utc>>,
    limits: ParseLimits,
) -> Result<ParsedSource> {
    file.seek(SeekFrom::Start(start.offset))?;
    let reader = BufReader::new(file);
    let context = ParseContext {
        adapter,
        path,
        since,
        limits,
    };
    parse_reader(&context, reader, start.offset, start.line, state)
}

fn parse_compressed(
    adapter: &dyn SourceAdapter,
    mut file: File,
    path: &Path,
    since: Option<DateTime<Utc>>,
    limits: ParseLimits,
    window_log_max: u32,
) -> Result<ParsedSource> {
    file.seek(SeekFrom::Start(0))?;
    let mut decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("failed to open zstd source {}", path.display()))?;
    decoder
        .window_log_max(window_log_max)
        .context("failed to apply the bounded zstd window")?;
    let reader = BufReader::new(decoder);
    let mut state = Value::Null;
    let context = ParseContext {
        adapter,
        path,
        since,
        limits,
    };
    parse_reader(&context, reader, 0, 0, &mut state)
}

fn parse_reader<R: BufRead>(
    context: &ParseContext<'_>,
    mut reader: R,
    start_offset: u64,
    start_line: u64,
    state: &mut Value,
) -> Result<ParsedSource> {
    let limits = context.limits;
    let mut offset = start_offset;
    let mut line_number = start_line;
    let mut records = Vec::with_capacity(LINE_BATCH_SIZE);
    let mut batch_bytes = 0_usize;
    let mut observations = Vec::new();
    let mut warnings = Vec::new();
    let mut source_complete = true;
    let mut adapter_incomplete = false;
    let mut stop_warning = None;

    loop {
        if line_number.saturating_sub(start_line) >= limits.max_records
            || offset.saturating_sub(start_offset) >= limits.max_bytes
        {
            if reader.fill_buf()?.is_empty() {
                break;
            }
            source_complete = false;
            stop_warning = Some(resource_limit_warning(offset, line_number));
            break;
        }

        let line_start = offset;
        let mut bytes = Vec::new();
        let read = match read_bounded_line(&mut reader, &mut bytes, limits.max_line_bytes)? {
            BoundedLine::Eof => break,
            BoundedLine::Incomplete => {
                source_complete = false;
                stop_warning = Some(
                    ScanWarning::new(
                        "incomplete_tail",
                        "final JSONL record is incomplete and will be retried on the next scan",
                    )
                    .at(format!("byte {line_start}")),
                );
                break;
            }
            BoundedLine::TooLong => {
                source_complete = false;
                stop_warning = Some(
                    ScanWarning::new(
                        "source_line_limit",
                        "a source record exceeded the bounded line-size limit; the source remains incomplete",
                    )
                    .at(format!("line {} @ byte {line_start}", line_number + 1)),
                );
                break;
            }
            BoundedLine::Complete(read) => read,
        };
        if offset
            .saturating_sub(start_offset)
            .saturating_add(read as u64)
            > limits.max_bytes
        {
            source_complete = false;
            stop_warning = Some(resource_limit_warning(offset, line_number));
            break;
        }

        offset = offset.saturating_add(read as u64);
        line_number += 1;
        trim_newline(&mut bytes);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        batch_bytes = batch_bytes.saturating_add(text.len());
        records.push(LineRecord {
            line_number,
            byte_start: line_start,
            byte_end: offset,
            text,
        });
        if records.len() >= LINE_BATCH_SIZE || batch_bytes >= LINE_BATCH_BYTES {
            let batch_start_offset = records[0].byte_start;
            let batch_start_line = records[0].line_number.saturating_sub(1);
            match consume_batch(
                context,
                &mut records,
                state,
                &mut observations,
                &mut warnings,
            )? {
                BatchConsumption::Consumed { incomplete } => {
                    adapter_incomplete |= incomplete;
                    batch_bytes = 0;
                }
                BatchConsumption::Limited => {
                    offset = batch_start_offset;
                    line_number = batch_start_line;
                    source_complete = false;
                    stop_warning = Some(resource_limit_warning(offset, line_number));
                    break;
                }
            }
        }
    }

    if !records.is_empty() {
        let batch_start_offset = records[0].byte_start;
        let batch_start_line = records[0].line_number.saturating_sub(1);
        match consume_batch(
            context,
            &mut records,
            state,
            &mut observations,
            &mut warnings,
        )? {
            BatchConsumption::Consumed { incomplete } => adapter_incomplete |= incomplete,
            BatchConsumption::Limited => {
                offset = batch_start_offset;
                line_number = batch_start_line;
                source_complete = false;
                stop_warning = Some(resource_limit_warning(offset, line_number));
            }
        }
    }
    if let Some(warning) = stop_warning {
        warnings.push(warning);
    }
    source_complete &= !adapter_incomplete;
    let next_state = state.clone();
    Ok(ParsedSource {
        batch: ParseBatch {
            observations,
            warnings,
            next_state: next_state.clone(),
            incomplete: !source_complete,
        },
        checkpoint_offset: offset,
        checkpoint_line: line_number,
        next_state,
        complete: source_complete,
        // Adapter-level incompleteness means a record was skipped after the
        // durable checkpoint advanced. Rebuild from zero until the source is
        // corrected rather than silently preserving a permanent gap.
        force_rebuild: adapter_incomplete,
        records: line_number.saturating_sub(start_line),
    })
}

enum BoundedLine {
    Eof,
    Complete(usize),
    Incomplete,
    TooLong,
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    bytes: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<BoundedLine> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if bytes.is_empty() {
                BoundedLine::Eof
            } else {
                BoundedLine::Incomplete
            });
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if bytes.len().saturating_add(take) > max_bytes {
            return Ok(BoundedLine::TooLong);
        }
        let found_newline = available.get(take.saturating_sub(1)) == Some(&b'\n');
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if found_newline {
            return Ok(BoundedLine::Complete(bytes.len()));
        }
    }
}

enum BatchConsumption {
    Consumed { incomplete: bool },
    Limited,
}

fn consume_batch(
    context: &ParseContext<'_>,
    records: &mut Vec<LineRecord>,
    state: &mut Value,
    observations: &mut Vec<UsageObservation>,
    warnings: &mut Vec<ScanWarning>,
) -> Result<BatchConsumption> {
    if records.is_empty() {
        return Ok(BatchConsumption::Consumed { incomplete: false });
    }
    let batch = context
        .adapter
        .parse_lines(context.path, records, Some(state))?;
    let retained_observations = batch
        .observations
        .iter()
        .filter(|observation| {
            context
                .since
                .is_none_or(|cutoff| observation.occurred_at >= cutoff)
        })
        .count();
    if observations.len().saturating_add(retained_observations) > context.limits.max_observations
        || warnings.len().saturating_add(batch.warnings.len()) > context.limits.max_warnings
    {
        records.clear();
        return Ok(BatchConsumption::Limited);
    }
    let incomplete = batch.incomplete;
    *state = batch.next_state;
    observations.extend(batch.observations.into_iter().filter(|observation| {
        context
            .since
            .is_none_or(|cutoff| observation.occurred_at >= cutoff)
    }));
    warnings.extend(batch.warnings);
    records.clear();
    Ok(BatchConsumption::Consumed { incomplete })
}

fn resource_limit_warning(offset: u64, line_number: u64) -> ScanWarning {
    ScanWarning::new(
        "source_resource_limit",
        "source parsing reached a bounded resource limit; remaining records will be retried on a later scan",
    )
    .at(format!("line {} @ byte {offset}", line_number + 1))
}

fn compressed_archive_limit_warning() -> ScanWarning {
    ScanWarning::new(
        "compressed_archive_unsupported",
        "compressed source could not be parsed completely within supported integrity and resource bounds; no partial usage was imported",
    )
}

fn is_unchanged(
    source: &SourceSpec,
    existing: Option<&SourceCheckpoint>,
    file_size: u64,
    modified_ns: i64,
    content_hash: &str,
) -> bool {
    existing.is_some_and(|checkpoint| {
        checkpoint.client == source.client
            && checkpoint.compressed == source.compressed
            && checkpoint.file_size == file_size
            && checkpoint.modified_ns == modified_ns
            && checkpoint.content_hash == content_hash
            && !checkpoint_is_partial(checkpoint)
    })
}

fn requires_backfill(checkpoint: &SourceCheckpoint, current_size: u64) -> bool {
    current_size > 0 && checkpoint.checkpoint_offset == 0 && checkpoint.checkpoint_line == 0
}

fn checkpoint_is_partial(checkpoint: &SourceCheckpoint) -> bool {
    checkpoint.checkpoint_offset < checkpoint.file_size
}

fn checkpoint_still_valid_file(file: &File, checkpoint: &SourceCheckpoint) -> Result<bool> {
    if checkpoint.head_hash != hash_head_file(file, checkpoint.checkpoint_offset)? {
        return Ok(false);
    }
    Ok(checkpoint.checkpoint_hash
        == hash_window_ending_at_file(file, checkpoint.checkpoint_offset)?)
}

#[cfg(test)]
fn hash_head(path: &Path, checkpoint_offset: u64) -> Result<String> {
    let file = open_source_file(path)?;
    hash_head_file(&file, checkpoint_offset)
}

fn hash_head_file(file: &File, checkpoint_offset: u64) -> Result<String> {
    hash_exact_prefix_file(file, checkpoint_offset)
}

#[cfg(test)]
fn hash_window_ending_at(path: &Path, offset: u64) -> Result<String> {
    let file = open_source_file(path)?;
    hash_window_ending_at_file(&file, offset)
}

fn hash_window_ending_at_file(file: &File, offset: u64) -> Result<String> {
    let mut file = file.try_clone()?;
    let start = offset.saturating_sub(CHECKPOINT_WINDOW);
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = vec![0_u8; (offset - start) as usize];
    file.read_exact(&mut buffer)?;
    Ok(hex::encode(Sha256::digest(&buffer)))
}

/// Hash the complete physical source. The caller applies a strict physical
/// file-size ceiling before reaching this function, and the aggregate work
/// scheduler accounts for repeated full-file stability checks. Full digests
/// prevent a same-size, timestamp-preserving rewrite outside sample windows
/// from retaining stale accounting.
#[cfg(test)]
fn source_fingerprint(path: &Path) -> Result<SourceFingerprint> {
    let file = open_source_file(path)?;
    source_fingerprint_from_file(&file)
}

fn source_fingerprint_within(path: &Path, trusted_root: &Path) -> Result<SourceFingerprint> {
    let file = open_source_file_within(path, trusted_root)?;
    source_fingerprint_from_file(&file)
}

fn source_fingerprint_from_file(file: &File) -> Result<SourceFingerprint> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        anyhow::bail!("source is not a regular file");
    }
    let file_size = metadata.len();
    Ok(SourceFingerprint {
        file_size,
        modified_ns: modified_ns(&metadata),
        sample_hash: hash_exact_prefix_file(file, file_size)?,
        exhaustive: true,
    })
}

/// Copies one admitted source into an unlinked private temporary file and
/// fingerprints those exact bytes. Parsing the snapshot prevents concurrent
/// same-inode edits from injecting bytes between the integrity check and the
/// parser. The original handle is used only to capture and later validate the
/// source; observations are never parsed from a live, mutable file.
fn snapshot_source(
    source: &File,
    metadata_before: &Metadata,
    max_source_file_bytes: u64,
) -> Result<(File, SourceFingerprint)> {
    if metadata_before.len() > max_source_file_bytes {
        anyhow::bail!("source exceeded the bounded file-size limit");
    }
    let mut input = source.try_clone()?;
    input.seek(SeekFrom::Start(0))?;
    let mut snapshot = tempfile::tempfile().context("failed to create private source snapshot")?;
    let copied = io::copy(
        &mut input.take(max_source_file_bytes.saturating_add(1)),
        &mut snapshot,
    )?;
    if copied != metadata_before.len() || copied > max_source_file_bytes {
        anyhow::bail!("source changed size while its bounded snapshot was captured");
    }
    let metadata_after = source.metadata()?;
    if metadata_after.len() != metadata_before.len()
        || modified_ns(&metadata_after) != modified_ns(metadata_before)
    {
        anyhow::bail!("source changed while its bounded snapshot was captured");
    }
    snapshot.seek(SeekFrom::Start(0))?;
    let mut fingerprint = source_fingerprint_from_file(&snapshot)?;
    fingerprint.modified_ns = modified_ns(metadata_before);
    Ok((snapshot, fingerprint))
}

fn hash_exact_prefix_file(file: &File, length: u64) -> Result<String> {
    let mut file = file.try_clone()?;
    let mut hasher = Sha256::new();
    hasher.update(b"token-ledger-source-v3");
    hasher.update(length.to_le_bytes());
    hash_file_range(&mut file, 0, length, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Open a discovered source without traversing a final-component symbolic
/// link or Windows reparse point. The returned handle is the authority for
/// metadata, fingerprinting, checkpoint validation, and parsing.
fn open_source_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT makes the final component itself open;
        // the attribute check below then rejects every reparse-point class.
        options.custom_flags(0x0020_0000);
    }

    let file = options.open(path)?;
    let metadata = file.metadata()?;

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source is a reparse point",
            ));
        }
    }

    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }
    Ok(file)
}

fn open_source_file_within(path: &Path, trusted_root: &Path) -> io::Result<File> {
    validate_no_link_ancestors(path, trusted_root)?;
    let file = open_source_file(path)?;
    validate_no_link_ancestors(path, trusted_root)?;
    Ok(file)
}

/// Rejects symbolic-link and Windows reparse-point ancestors. The final
/// component is also protected by the no-follow open above. This is a
/// defense-in-depth pathname check; Token Ledger is not a sandbox for source
/// trees controlled by a concurrent hostile process.
fn validate_no_link_ancestors(path: &Path, trusted_root: &Path) -> io::Result<()> {
    if !path.starts_with(trusted_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source escaped its trusted discovery root",
        ));
    }
    for ancestor in path.ancestors().skip(1) {
        if ancestor == trusted_root {
            return Ok(());
        }
        if ancestor.as_os_str().is_empty() {
            break;
        }
        let metadata = std::fs::symlink_metadata(ancestor)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source has a symbolic-link ancestor",
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source has a reparse-point ancestor",
                ));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "source did not descend from its trusted discovery root",
    ))
}

fn hash_file_range(file: &mut File, start: u64, length: u64, hasher: &mut Sha256) -> Result<()> {
    file.seek(SeekFrom::Start(start))?;
    let mut remaining = length;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64))
            .expect("bounded hash buffer length fits usize");
        file.read_exact(&mut buffer[..requested])?;
        hasher.update(&buffer[..requested]);
        remaining -= requested as u64;
    }
    Ok(())
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
    use rusqlite::Connection;
    use std::fs::{FileTimes, OpenOptions};
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::{NamedTempFile, tempdir};

    use crate::adapters::DiscoveryResult;
    use crate::adapters::claude::ClaudeAdapter;

    fn create_test_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file symlinks are unsupported on this platform",
            ))
        }
    }

    fn try_create_test_file_symlink(target: &Path, link: &Path) -> Result<bool> {
        match create_test_file_symlink(target, link) {
            Ok(()) => Ok(true),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                ) || error.raw_os_error() == Some(1314) =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn create_test_dir_symlink(target: &Path, link: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "directory symlinks are unsupported on this platform",
            ))
        }
    }

    #[test]
    fn source_with_linked_ancestor_is_rejected() -> Result<()> {
        let directory = tempdir()?;
        let real_parent = directory.path().join("real");
        std::fs::create_dir(&real_parent)?;
        std::fs::write(real_parent.join("source.jsonl"), b"{}\n")?;
        let linked_parent = directory.path().join("linked");
        match create_test_dir_symlink(&real_parent, &linked_parent) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                ) || error.raw_os_error() == Some(1314) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
        let error = open_source_file_within(&linked_parent.join("source.jsonl"), directory.path())
            .expect_err("linked ancestor must be rejected");
        assert!(error.to_string().contains("ancestor"));
        Ok(())
    }

    #[test]
    fn trusted_root_may_be_an_operating_system_alias() -> Result<()> {
        let directory = tempdir()?;
        let real_root = directory.path().join("private-root");
        std::fs::create_dir(&real_root)?;
        std::fs::write(real_root.join("source.jsonl"), b"{}\n")?;
        let trusted_alias = directory.path().join("system-alias");
        match create_test_dir_symlink(&real_root, &trusted_alias) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                ) || error.raw_os_error() == Some(1314) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }

        let file = open_source_file_within(&trusted_alias.join("source.jsonl"), &trusted_alias)?;
        assert_eq!(file.metadata()?.len(), 3);
        Ok(())
    }

    #[test]
    fn immutable_snapshot_digest_matches_the_bytes_that_are_parsed() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("source.jsonl");
        std::fs::write(&path, b"original\n")?;
        let source = open_source_file(&path)?;
        let metadata = source.metadata()?;
        let original_mtime = metadata.modified()?;
        let (snapshot, snapshot_fingerprint) =
            snapshot_source(&source, &metadata, MAX_SOURCE_FILE_BYTES)?;

        std::fs::write(&path, b"injected\n")?;
        let rewritten = OpenOptions::new().write(true).open(&path)?;
        rewritten.set_times(FileTimes::new().set_modified(original_mtime))?;

        assert_eq!(
            snapshot_fingerprint.sample_hash,
            source_fingerprint_from_file(&snapshot)?.sample_hash
        );
        assert_ne!(
            snapshot_fingerprint.sample_hash,
            source_fingerprint(&path)?.sample_hash
        );
        Ok(())
    }

    #[test]
    fn stability_retries_consume_additional_aggregate_work() {
        let mut remaining = 10;
        let mut reservation = SourceWorkReservation {
            reserved_bytes: 10,
            remaining_bytes: &mut remaining,
        };
        assert!(reservation.admit_attempt(10, 0));
        assert!(reservation.admit_attempt(10, 1));
        assert!(!reservation.admit_attempt(10, 2));
        assert_eq!(remaining, 0);
    }

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

    struct StaticAdapter {
        sources: Vec<SourceSpec>,
    }

    impl SourceAdapter for StaticAdapter {
        fn client(&self) -> Client {
            Client::ClaudeCode
        }

        fn display_name(&self) -> &'static str {
            "static test"
        }

        fn discover_bounded(
            &self,
            _config: &Config,
            _request: DiscoveryRequest,
        ) -> Result<DiscoveryResult> {
            Ok(DiscoveryResult::complete(self.sources.clone()))
        }

        fn parse_lines(
            &self,
            path: &Path,
            lines: &[LineRecord],
            previous_state: Option<&Value>,
        ) -> Result<ParseBatch> {
            ClaudeAdapter.parse_lines(path, lines, previous_state)
        }
    }

    struct ClientStaticAdapter {
        client: Client,
        sources: Vec<SourceSpec>,
    }

    impl SourceAdapter for ClientStaticAdapter {
        fn client(&self) -> Client {
            self.client
        }

        fn display_name(&self) -> &'static str {
            "client-static test"
        }

        fn discover_bounded(
            &self,
            _config: &Config,
            _request: DiscoveryRequest,
        ) -> Result<DiscoveryResult> {
            Ok(DiscoveryResult::complete(self.sources.clone()))
        }

        fn parse_lines(
            &self,
            _path: &Path,
            _lines: &[LineRecord],
            previous_state: Option<&Value>,
        ) -> Result<ParseBatch> {
            Ok(ParseBatch {
                next_state: previous_state.cloned().unwrap_or(Value::Null),
                ..Default::default()
            })
        }
    }

    fn client_static_adapter(client: Client, path: PathBuf) -> Box<dyn SourceAdapter> {
        Box::new(ClientStaticAdapter {
            client,
            sources: vec![SourceSpec {
                trusted_root: path.parent().expect("test source parent").to_path_buf(),
                path,
                client,
                compressed: false,
            }],
        })
    }

    fn static_adapters(path: PathBuf, compressed: bool) -> Vec<Box<dyn SourceAdapter>> {
        vec![Box::new(StaticAdapter {
            sources: vec![SourceSpec {
                trusted_root: path.parent().expect("test source parent").to_path_buf(),
                path,
                client: Client::ClaudeCode,
                compressed,
            }],
        })]
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

        fn discover_bounded(
            &self,
            _config: &Config,
            _request: DiscoveryRequest,
        ) -> Result<DiscoveryResult> {
            Ok(DiscoveryResult::complete(vec![SourceSpec {
                path: self.source.clone(),
                trusted_root: self
                    .source
                    .parent()
                    .expect("test source parent")
                    .to_path_buf(),
                client: Client::ClaudeCode,
                compressed: false,
            }]))
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

        fn discover_bounded(
            &self,
            _config: &Config,
            _request: DiscoveryRequest,
        ) -> Result<DiscoveryResult> {
            Ok(DiscoveryResult::complete(
                self.sources
                    .iter()
                    .map(|path| SourceSpec {
                        path: path.clone(),
                        trusted_root: path.parent().expect("test source parent").to_path_buf(),
                        client: Client::ClaudeCode,
                        compressed: false,
                    })
                    .collect(),
            ))
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
    fn same_size_rewrite_preserving_head_tail_and_mtime_forces_reset() -> Result<()> {
        let dir = tempdir()?;
        let source = dir.path().join("rewrite.jsonl");
        let mut records = (0..120)
            .map(|index| claude_record(&format!("message-{index:03}"), 1))
            .collect::<Vec<_>>();
        let original = records.concat();
        assert!(original.len() > (CHECKPOINT_WINDOW as usize * 2));
        std::fs::write(&source, &original)?;
        let original_modified = std::fs::metadata(&source)?.modified()?;

        let adapters = static_adapters(source.clone(), false);
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let first = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert!(!first.provisional);
        assert_eq!(ledger.canonical_events(None, None)?.len(), 120);

        records[60] = claude_record("message-060", 9);
        let rewritten = records.concat();
        assert_eq!(rewritten.len(), original.len());
        assert_eq!(
            &rewritten[..CHECKPOINT_WINDOW as usize],
            &original[..CHECKPOINT_WINDOW as usize]
        );
        assert_eq!(
            &rewritten[rewritten.len() - CHECKPOINT_WINDOW as usize..],
            &original[original.len() - CHECKPOINT_WINDOW as usize..]
        );
        std::fs::write(&source, rewritten)?;
        OpenOptions::new()
            .write(true)
            .open(&source)?
            .set_times(FileTimes::new().set_modified(original_modified))?;

        let second = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(second.scanned_sources, 1);
        assert_eq!(second.unchanged_sources, 0);
        assert_eq!(second.reset_sources, 1);
        assert!(!second.provisional);
        let events = ledger.canonical_events(None, None)?;
        assert_eq!(events.len(), 120);
        assert_eq!(
            events
                .iter()
                .map(|event| event.usage.output_tokens_total)
                .sum::<u64>(),
            128
        );
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
        assert_eq!(first.incomplete_source_count, 0);
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
    fn incomplete_tail_remains_provisional_and_is_retried() -> Result<()> {
        let dir = tempdir()?;
        let source = dir.path().join("incomplete.jsonl");
        let complete_prefix = claude_record("first", 1);
        let incomplete_tail = claude_record("second", 2)
            .strip_suffix('\n')
            .expect("fixture record ends in newline")
            .to_string();
        std::fs::write(&source, format!("{complete_prefix}{incomplete_tail}"))?;
        let adapters = static_adapters(source.clone(), false);
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        let first = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(first.scanned_sources, 1);
        assert_eq!(first.observations, 1);
        assert_eq!(first.active_or_volatile_source_count, 0);
        assert_eq!(first.incomplete_source_count, 1);
        assert!(first.provisional);
        let checkpoint = ledger
            .source_checkpoint(&source)?
            .context("incomplete source checkpoint missing")?;
        assert_eq!(checkpoint.checkpoint_offset, complete_prefix.len() as u64);
        assert!(checkpoint.checkpoint_offset < checkpoint.file_size);

        // Identical size and mtime must not turn the partial checkpoint into an
        // "unchanged" full-coverage claim.
        let second = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(second.scanned_sources, 1);
        assert_eq!(second.unchanged_sources, 0);
        assert_eq!(second.active_or_volatile_source_count, 0);
        assert_eq!(second.incomplete_source_count, 1);
        assert!(second.provisional);
        assert_eq!(ledger.canonical_events(None, None)?.len(), 1);

        OpenOptions::new()
            .append(true)
            .open(&source)?
            .write_all(b"\n")?;
        let third = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(third.scanned_sources, 1);
        assert!(!third.provisional);
        assert_eq!(ledger.canonical_events(None, None)?.len(), 2);
        Ok(())
    }

    #[test]
    fn adapter_skipped_record_forces_rebuild_and_partial_coverage() -> Result<()> {
        const SECRET: &str = "MALFORMED_TRANSCRIPT_SECRET_MUST_NOT_ESCAPE";

        let dir = tempdir()?;
        let source = dir.path().join("malformed.jsonl");
        std::fs::write(&source, format!("{}{SECRET}\n", claude_record("known", 1)))?;
        let adapters = static_adapters(source.clone(), false);
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        let summary = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(summary.observations, 1);
        assert_eq!(summary.active_or_volatile_source_count, 0);
        assert_eq!(summary.incomplete_source_count, 1);
        assert!(summary.provisional);
        let checkpoint = ledger
            .source_checkpoint(&source)?
            .context("malformed source checkpoint missing")?;
        assert_eq!(checkpoint.checkpoint_offset, 0);
        assert!(checkpoint.adapter_state.is_null());
        assert!(
            ledger
                .warning_code_counts()?
                .iter()
                .any(|warning| warning.code == "claude_malformed_record")
        );
        Ok(())
    }

    #[test]
    fn oversized_line_is_bounded_and_warning_does_not_leak_content() -> Result<()> {
        const SECRET: &str = "TRANSCRIPT_SECRET_MUST_NOT_ESCAPE_RESOURCE_WARNING";

        let dir = tempdir()?;
        let source = dir.path().join("oversized.jsonl");
        std::fs::write(&source, format!("{SECRET}{SECRET}{SECRET}\n"))?;
        let database = dir.path().join("ledger.sqlite");
        let adapters = static_adapters(source.clone(), false);
        let mut ledger = Ledger::open(&database)?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.plain.max_line_bytes = 32;

        let summary = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert!(summary.provisional);
        assert_eq!(summary.active_or_volatile_source_count, 0);
        assert_eq!(summary.incomplete_source_count, 1);
        assert_eq!(summary.observations, 0);
        let checkpoint = ledger
            .source_checkpoint(&source)?
            .context("limited source checkpoint missing")?;
        assert_eq!(checkpoint.checkpoint_offset, 0);

        let connection = Connection::open(database)?;
        let (code, message, locator): (String, String, Option<String>) = connection.query_row(
            "SELECT code, message, locator FROM scan_warnings ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(code, "source_line_limit");
        assert!(!message.contains(SECRET));
        assert!(!locator.unwrap_or_default().contains(SECRET));
        assert_eq!(
            ledger
                .coverage_snapshot()?
                .last_scan
                .as_ref()
                .map(|scan| scan.status.as_str()),
            Some("partial")
        );
        let coverage = ledger.client_coverage(Client::ClaudeCode)?;
        assert!(coverage.last_successful_source_scan.is_none());
        Ok(())
    }

    #[test]
    fn decompressed_byte_limit_hard_stops_without_importing_a_prefix() -> Result<()> {
        let dir = tempdir()?;
        let source = dir.path().join("archive.jsonl.zst");
        let first_record = claude_record("first", 1);
        let payload = format!(
            "{first_record}{}{}",
            claude_record("second", 2),
            claude_record("third", 3)
        );
        std::fs::write(&source, zstd::stream::encode_all(payload.as_bytes(), 0)?)?;
        let adapters = static_adapters(source.clone(), true);
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.compressed.max_bytes = first_record.len() as u64;

        let summary = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(summary.scanned_sources, 1);
        assert_eq!(summary.observations, 0);
        assert!(summary.provisional);
        assert_eq!(summary.active_or_volatile_source_count, 0);
        assert_eq!(summary.incomplete_source_count, 1);
        assert!(ledger.canonical_events(None, None)?.is_empty());
        let checkpoint = ledger
            .source_checkpoint(&source)?
            .context("compressed checkpoint missing")?;
        assert_eq!(checkpoint.checkpoint_offset, 0);
        assert_eq!(
            checkpoint
                .adapter_state
                .get("compressed_archive_unsupported")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            ledger
                .warning_code_counts()?
                .iter()
                .any(|warning| warning.code == "compressed_archive_unsupported")
        );

        let repeated = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(repeated.scanned_sources, 0);
        assert_eq!(repeated.observations, 0);
        assert!(repeated.provisional);
        assert!(ledger.canonical_events(None, None)?.is_empty());
        Ok(())
    }

    #[test]
    fn compressed_line_limit_hard_stops_without_importing_a_prefix() -> Result<()> {
        let dir = tempdir()?;
        let source = dir.path().join("oversized-line.jsonl.zst");
        let valid_prefix = claude_record("valid-prefix", 1);
        let max_line_bytes = valid_prefix.len() + 16;
        let payload = format!("{valid_prefix}{}\n", "x".repeat(max_line_bytes + 1));
        std::fs::write(&source, zstd::stream::encode_all(payload.as_bytes(), 0)?)?;
        let adapters = static_adapters(source.clone(), true);
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.compressed.max_line_bytes = max_line_bytes;

        let first = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(first.scanned_sources, 1);
        assert_eq!(first.observations, 0);
        assert!(first.provisional);
        assert!(ledger.canonical_events(None, None)?.is_empty());
        let checkpoint = ledger
            .source_checkpoint(&source)?
            .context("compressed checkpoint missing")?;
        assert_eq!(checkpoint.checkpoint_offset, 0);
        assert_eq!(
            checkpoint
                .adapter_state
                .get("compressed_archive_unsupported")
                .and_then(Value::as_bool),
            Some(true)
        );

        let repeated = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(repeated.scanned_sources, 0);
        assert_eq!(repeated.observations, 0);
        assert!(ledger.canonical_events(None, None)?.is_empty());
        Ok(())
    }

    #[test]
    fn opened_handle_cannot_exceed_scheduled_work_reservation() -> Result<()> {
        let dir = tempdir()?;
        let source_path = dir.path().join("grew-after-scheduling.jsonl");
        std::fs::write(&source_path, claude_record("small", 1))?;
        let source = SourceSpec {
            path: source_path.clone(),
            trusted_root: dir.path().to_path_buf(),
            client: Client::ClaudeCode,
            compressed: false,
        };
        let limits = DEFAULT_SCAN_LIMITS;
        let reserved = estimate_source_work(&source, limits)?;
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&source_path)?
            .set_len(16 * 1024 * 1024)?;
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let adapter = StaticAdapter {
            sources: vec![source.clone()],
        };
        let mut remaining_work = 0;
        let mut work_reservation = SourceWorkReservation {
            reserved_bytes: reserved,
            remaining_bytes: &mut remaining_work,
        };

        match scan_one(
            &mut ledger,
            &adapter,
            &source,
            &ScanOptions {
                dry_run: true,
                ..Default::default()
            },
            None,
            limits,
            &mut work_reservation,
        )? {
            SourceOutcome::Limited(warning) => assert_eq!(warning.code, "scan_resource_limit"),
            _ => anyhow::bail!("a grown source bypassed its work reservation"),
        }
        assert!(ledger.canonical_events(None, None)?.is_empty());
        Ok(())
    }

    #[test]
    fn maximum_admitted_source_fits_aggregate_work_budget() -> Result<()> {
        let limits = DEFAULT_SCAN_LIMITS;
        let estimate = estimate_source_work_for_size(limits.max_source_file_bytes, true, limits);
        assert!(estimate <= limits.max_total_work_bytes);
        Ok(())
    }

    #[test]
    fn file_and_discovery_limits_are_reported_as_partial() -> Result<()> {
        let dir = tempdir()?;
        let first = dir.path().join("a.jsonl");
        let second = dir.path().join("b.jsonl");
        std::fs::write(&first, claude_record("first", 1))?;
        std::fs::write(&second, claude_record("second", 2))?;
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
            sources: vec![first.clone(), second]
                .into_iter()
                .map(|path| SourceSpec {
                    trusted_root: path.parent().expect("test source parent").to_path_buf(),
                    path,
                    client: Client::ClaudeCode,
                    compressed: false,
                })
                .collect(),
        })];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_sources_per_adapter = 1;
        limits.max_source_file_bytes = 1;

        let summary = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(summary.discovered_sources, 2);
        assert_eq!(summary.scanned_sources, 0);
        assert_eq!(summary.active_or_volatile_source_count, 0);
        assert_eq!(summary.incomplete_source_count, 1);
        assert!(summary.coverage_limited);
        assert!(summary.provisional);
        let codes = ledger.warning_code_counts()?;
        assert!(
            codes
                .iter()
                .any(|warning| warning.code == "source_discovery_limit")
        );
        assert!(
            codes
                .iter()
                .any(|warning| warning.code == "source_file_limit")
        );
        Ok(())
    }

    #[test]
    fn built_in_entry_cap_scans_safe_partial_sources_and_stays_provisional() -> Result<()> {
        let dir = tempdir()?;
        let claude_root = dir.path().join("claude-entry-cap");
        let projects = claude_root.join("projects");
        std::fs::create_dir_all(&projects)?;
        for index in 0..3 {
            std::fs::write(
                projects.join(format!("{index}.jsonl")),
                claude_record(&format!("entry-{index}"), 1),
            )?;
        }
        let config = Config {
            claude_root: Some(claude_root),
            ..Default::default()
        };
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ClaudeAdapter)];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_sources_per_adapter = 4;
        limits.max_discovery_entries_per_adapter = 2;

        let summary = scan_with_limits(
            &mut ledger,
            &config,
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(summary.discovered_sources, 2);
        assert_eq!(summary.scanned_sources, 2);
        assert_eq!(ledger.source_rows()?.len(), 2);
        assert!(summary.coverage_limited);
        assert!(summary.provisional);
        assert!(
            ledger
                .warning_code_counts()?
                .iter()
                .any(|warning| warning.code == "discovery_entry_limit")
        );
        Ok(())
    }

    #[test]
    fn dry_run_discovery_is_seed_zero_deterministic_and_provisional() -> Result<()> {
        let dir = tempdir()?;
        let claude_root = dir.path().join("claude-dry-cap");
        let projects = claude_root.join("projects");
        std::fs::create_dir_all(&projects)?;
        for index in 0..5 {
            std::fs::write(
                projects.join(format!("{index}.jsonl")),
                claude_record(&format!("dry-{index}"), 1),
            )?;
        }
        let config = Config {
            claude_root: Some(claude_root),
            ..Default::default()
        };
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ClaudeAdapter)];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_sources_per_adapter = 2;
        limits.max_discovery_entries_per_adapter = 10;
        let options = ScanOptions {
            dry_run: true,
            ..Default::default()
        };

        let first = scan_with_limits(&mut ledger, &config, &adapters, &options, limits)?;
        let second = scan_with_limits(&mut ledger, &config, &adapters, &options, limits)?;
        assert_eq!(first.discovered_sources, 5);
        assert_eq!(first.scanned_sources, 2);
        assert_eq!(first.discovered_sources, second.discovered_sources);
        assert_eq!(first.scanned_sources, second.scanned_sources);
        assert_eq!(first.observations, second.observations);
        assert_eq!(first.warnings, second.warnings);
        assert!(first.coverage_limited && second.coverage_limited);
        assert!(first.provisional && second.provisional);
        assert!(ledger.source_rows()?.is_empty());
        Ok(())
    }

    #[test]
    fn built_in_source_candidate_cap_rotates_pages_across_scans() -> Result<()> {
        let dir = tempdir()?;
        let claude_root = dir.path().join("claude-source-cap");
        let projects = claude_root.join("projects");
        std::fs::create_dir_all(&projects)?;
        for index in 0..5 {
            std::fs::write(
                projects.join(format!("{index}.jsonl")),
                claude_record(&format!("candidate-{index}"), 1),
            )?;
        }
        let config = Config {
            claude_root: Some(claude_root),
            ..Default::default()
        };
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ClaudeAdapter)];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_sources_per_adapter = 2;
        limits.max_discovery_entries_per_adapter = 10;

        for expected_sources in [2, 4, 5] {
            let summary = scan_with_limits(
                &mut ledger,
                &config,
                &adapters,
                &ScanOptions::default(),
                limits,
            )?;
            assert_eq!(summary.discovered_sources, 5);
            assert!(summary.coverage_limited);
            assert!(summary.provisional);
            assert_eq!(ledger.source_rows()?.len(), expected_sources);
        }
        assert!(
            ledger
                .warning_code_counts()?
                .iter()
                .any(|warning| warning.code == "discovery_source_limit")
        );
        Ok(())
    }

    #[test]
    fn capped_source_windows_advance_without_adjacent_overlap() -> Result<()> {
        let dir = tempdir()?;
        let sources = (0..5)
            .map(|index| {
                let path = dir.path().join(format!("{index}.jsonl"));
                std::fs::write(&path, claude_record(&format!("message-{index}"), 1))?;
                Ok(SourceSpec {
                    trusted_root: path.parent().expect("test source parent").to_path_buf(),
                    path,
                    client: Client::ClaudeCode,
                    compressed: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter { sources })];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_sources_per_adapter = 2;
        limits.max_total_sources = 2;

        let first = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(first.scanned_sources, 2);
        assert_eq!(ledger.source_rows()?.len(), 2);

        let second = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(second.scanned_sources, 2);
        assert_eq!(second.unchanged_sources, 0);
        assert_eq!(ledger.source_rows()?.len(), 4);

        let third = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(third.scanned_sources, 1);
        assert_eq!(third.unchanged_sources, 1);
        assert_eq!(ledger.source_rows()?.len(), 5);
        Ok(())
    }

    #[test]
    fn work_limited_rotation_eventually_makes_every_source_first() -> Result<()> {
        let dir = tempdir()?;
        let sources = (0..5)
            .map(|index| {
                let path = dir.path().join(format!("work-{index}.jsonl"));
                std::fs::write(&path, claude_record(&format!("message-{index}"), 1))?;
                Ok(SourceSpec {
                    trusted_root: path.parent().expect("test source parent").to_path_buf(),
                    path,
                    client: Client::ClaudeCode,
                    compressed: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_sources_per_adapter = 2;
        limits.max_total_sources = 2;
        limits.max_total_work_bytes = estimate_source_work(&sources[0], limits)?;
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter { sources })];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        for _ in 0..12 {
            let summary = scan_with_limits(
                &mut ledger,
                &Config::default(),
                &adapters,
                &ScanOptions::default(),
                limits,
            )?;
            assert_eq!(summary.scanned_sources + summary.unchanged_sources, 1);
            if ledger.source_rows()?.len() == 5 {
                break;
            }
        }
        assert_eq!(ledger.source_rows()?.len(), 5);
        Ok(())
    }

    #[test]
    fn aggregate_record_and_observation_budgets_are_bounded_and_retryable() -> Result<()> {
        let dir = tempdir()?;
        let sources = (0..2)
            .map(|index| {
                let path = dir.path().join(format!("count-{index}.jsonl"));
                std::fs::write(&path, claude_record(&format!("message-{index}"), 1))?;
                Ok(SourceSpec {
                    trusted_root: path.parent().expect("test source parent").to_path_buf(),
                    path,
                    client: Client::ClaudeCode,
                    compressed: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter { sources })];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_total_records = 1;
        limits.max_total_observations = 1;
        limits.max_total_warnings = 10;

        for expected_sources in 1..=2 {
            let summary = scan_with_limits(
                &mut ledger,
                &Config::default(),
                &adapters,
                &ScanOptions::default(),
                limits,
            )?;
            assert_eq!(summary.scanned_sources, 1);
            assert_eq!(summary.observations, 1);
            assert!(summary.warnings <= limits.max_total_warnings as u64);
            assert!(summary.coverage_limited);
            assert!(summary.provisional);
            assert_eq!(ledger.source_rows()?.len(), expected_sources);
        }
        assert_eq!(ledger.canonical_events(None, None)?.len(), 2);
        Ok(())
    }

    #[test]
    fn aggregate_warning_budget_is_bounded_and_retryable() -> Result<()> {
        let dir = tempdir()?;
        let sources = (0..2)
            .map(|index| {
                let path = dir.path().join(format!("warning-{index}.jsonl"));
                std::fs::write(&path, "not-json\n")?;
                Ok(SourceSpec {
                    trusted_root: path.parent().expect("test source parent").to_path_buf(),
                    path,
                    client: Client::ClaudeCode,
                    compressed: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let database = dir.path().join("ledger.sqlite");
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter { sources })];
        let mut ledger = Ledger::open(&database)?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_total_warnings = 1;

        for _ in 0..4 {
            let summary = scan_with_limits(
                &mut ledger,
                &Config::default(),
                &adapters,
                &ScanOptions::default(),
                limits,
            )?;
            assert_eq!(summary.scanned_sources, 1);
            assert_eq!(summary.warnings, 1);
            assert!(summary.coverage_limited);
            assert!(summary.provisional);
            if ledger.source_rows()?.len() == 2 {
                break;
            }
        }
        assert_eq!(ledger.source_rows()?.len(), 2);

        let connection = Connection::open(database)?;
        let max_warnings_per_run: i64 = connection.query_row(
            "SELECT COALESCE(MAX(warning_count), 0) FROM (SELECT COUNT(*) AS warning_count FROM scan_warnings GROUP BY scan_run_id)",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(max_warnings_per_run, 1);
        Ok(())
    }

    #[test]
    fn aggregate_work_budget_rotates_adapter_priority_between_runs() -> Result<()> {
        let dir = tempdir()?;
        let claude = dir.path().join("claude.jsonl");
        let codex = dir.path().join("codex.jsonl");
        std::fs::write(&claude, "{}\n")?;
        std::fs::write(&codex, "{}\n")?;
        let adapters = vec![
            client_static_adapter(Client::ClaudeCode, claude.clone()),
            client_static_adapter(Client::OpenaiCodex, codex.clone()),
        ];
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_total_work_bytes = estimate_source_work(
            &SourceSpec {
                path: claude,
                trusted_root: dir.path().to_path_buf(),
                client: Client::ClaudeCode,
                compressed: false,
            },
            limits,
        )?;
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        for _ in 0..2 {
            let summary = scan_with_limits(
                &mut ledger,
                &Config::default(),
                &adapters,
                &ScanOptions::default(),
                limits,
            )?;
            assert_eq!(summary.scanned_sources, 1);
            assert!(summary.coverage_limited);
        }
        let clients = ledger
            .source_rows()?
            .into_iter()
            .map(|source| source.client)
            .collect::<HashSet<_>>();
        assert_eq!(clients, Client::ALL.into_iter().collect());
        Ok(())
    }

    #[test]
    fn checkpoint_aware_windows_advance_past_known_sources_below_source_cap() -> Result<()> {
        let dir = tempdir()?;
        let claude_sources = (0..3)
            .map(|index| {
                let path = dir.path().join(format!("claude-window-{index:02}.jsonl"));
                std::fs::write(&path, "{}\n")?;
                Ok(SourceSpec {
                    trusted_root: path.parent().expect("test source parent").to_path_buf(),
                    path,
                    client: Client::ClaudeCode,
                    compressed: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let codex_sources = (0..12)
            .map(|index| {
                let path = dir.path().join(format!("codex-window-{index:02}.jsonl"));
                std::fs::write(&path, "{}\n")?;
                Ok(SourceSpec {
                    trusted_root: path.parent().expect("test source parent").to_path_buf(),
                    path,
                    client: Client::OpenaiCodex,
                    compressed: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        // Seed every Claude source and the lexicographic prefix of Codex. This
        // reproduces the real plateau: a one-path rotation repeatedly spends
        // the conservative work budget revalidating already-known sources.
        let seed_adapters: Vec<Box<dyn SourceAdapter>> = vec![
            Box::new(ClientStaticAdapter {
                client: Client::ClaudeCode,
                sources: claude_sources.clone(),
            }),
            Box::new(ClientStaticAdapter {
                client: Client::OpenaiCodex,
                sources: codex_sources[..6].to_vec(),
            }),
        ];
        let seeded = scan(
            &mut ledger,
            &Config::default(),
            &seed_adapters,
            &ScanOptions::default(),
        )?;
        assert!(!seeded.provisional);
        assert_eq!(ledger.source_rows()?.len(), 9);

        let adapters: Vec<Box<dyn SourceAdapter>> = vec![
            Box::new(ClientStaticAdapter {
                client: Client::ClaudeCode,
                sources: claude_sources,
            }),
            Box::new(ClientStaticAdapter {
                client: Client::OpenaiCodex,
                sources: codex_sources.clone(),
            }),
        ];
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_sources_per_adapter = 4_096;
        limits.max_total_sources = 3;
        limits.max_total_work_bytes = estimate_source_work(&codex_sources[0], limits)? * 3;

        let mut prior_count = 9;
        for iteration in 0..5 {
            let summary = scan_with_limits(
                &mut ledger,
                &Config::default(),
                &adapters,
                &ScanOptions::default(),
                limits,
            )?;
            assert!(summary.coverage_limited);
            let current_count = ledger.source_rows()?.len();
            if iteration % 2 == 0 {
                assert!(
                    current_count > prior_count,
                    "Codex-first windows must advance into unseen sources"
                );
            }
            prior_count = current_count;
        }
        assert_eq!(ledger.source_rows()?.len(), 15);
        Ok(())
    }

    #[test]
    fn explicit_missing_claude_root_is_a_sanitized_partial_discovery() -> Result<()> {
        const SECRET_ROOT: &str = "PRIVATE_MISSING_CLAUDE_ROOT_CANARY";

        let dir = tempdir()?;
        let missing_root = dir.path().join(SECRET_ROOT);
        let database = dir.path().join("ledger.sqlite");
        let config = Config {
            claude_root_override: Some(missing_root.clone()),
            ..Default::default()
        };
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ClaudeAdapter)];
        let mut ledger = Ledger::open(&database)?;

        let summary = scan(&mut ledger, &config, &adapters, &ScanOptions::default())?;
        assert_eq!(summary.discovered_sources, 0);
        assert!(summary.coverage_limited);
        assert!(summary.provisional);

        let connection = Connection::open(database)?;
        let (code, message): (String, String) = connection.query_row(
            "SELECT code, message FROM scan_warnings ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(code, "discovery_failed");
        assert!(!message.contains(SECRET_ROOT));
        assert!(!message.contains(&missing_root.to_string_lossy().to_string()));
        Ok(())
    }

    #[test]
    fn symlink_candidate_is_unfollowed_provisional_and_path_private() -> Result<()> {
        const TARGET_CANARY: &str = "PRIVATE_SYMLINK_TARGET_CANARY.jsonl";
        const LINK_CANARY: &str = "PRIVATE_SYMLINK_LINK_CANARY.jsonl";

        let dir = tempdir()?;
        let outside = tempdir()?;
        let claude_root = dir.path().join("claude-symlink");
        let projects = claude_root.join("projects");
        std::fs::create_dir_all(&projects)?;
        let real_source = projects.join("real.jsonl");
        let outside_target = outside.path().join(TARGET_CANARY);
        let linked_source = projects.join(LINK_CANARY);
        std::fs::write(&real_source, claude_record("real", 1))?;
        std::fs::write(&outside_target, claude_record("outside", 99))?;
        if !try_create_test_file_symlink(&outside_target, &linked_source)? {
            return Ok(());
        }

        let database = dir.path().join("ledger.sqlite");
        let config = Config {
            claude_root: Some(claude_root),
            ..Default::default()
        };
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(ClaudeAdapter)];
        let mut ledger = Ledger::open(&database)?;
        let summary = scan(&mut ledger, &config, &adapters, &ScanOptions::default())?;

        assert_eq!(summary.discovered_sources, 1);
        assert_eq!(summary.scanned_sources, 1);
        assert!(summary.coverage_limited);
        assert!(summary.provisional);
        assert_eq!(ledger.canonical_events(None, None)?.len(), 1);
        assert!(ledger.source_checkpoint(&real_source)?.is_some());
        assert!(ledger.source_checkpoint(&linked_source)?.is_none());
        assert!(ledger.source_checkpoint(&outside_target)?.is_none());

        let connection = Connection::open(database)?;
        let (code, message): (String, String) = connection.query_row(
            "SELECT code, message FROM scan_warnings ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(code, "discovery_partial");
        assert!(!message.contains(TARGET_CANARY));
        assert!(!message.contains(LINK_CANARY));
        assert!(!message.contains(&outside_target.to_string_lossy().to_string()));
        Ok(())
    }

    #[test]
    fn scanner_rejects_a_symlink_even_when_adapter_returns_it() -> Result<()> {
        let dir = tempdir()?;
        let outside = tempdir()?;
        let target = outside.path().join("target.jsonl");
        let link = dir.path().join("swapped.jsonl");
        std::fs::write(&target, claude_record("must-not-import", 99))?;
        if !try_create_test_file_symlink(&target, &link)? {
            return Ok(());
        }
        let adapters = static_adapters(link.clone(), false);
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        let summary = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(summary.observations, 0);
        assert!(summary.provisional);
        assert!(ledger.canonical_events(None, None)?.is_empty());
        let rejected = ledger
            .source_checkpoint(&link)?
            .context("rejected source warning should retain a bounded source reference")?;
        assert_eq!(rejected.file_size, 0);
        assert_eq!(rejected.checkpoint_offset, 0);
        assert!(ledger.source_checkpoint(&target)?.is_none());
        Ok(())
    }

    #[test]
    fn scanner_rejects_regular_path_swapped_to_symlink_after_reservation() -> Result<()> {
        let dir = tempdir()?;
        let outside = tempdir()?;
        let source_path = dir.path().join("scheduled.jsonl");
        let target = outside.path().join("replacement.jsonl");
        std::fs::write(&source_path, claude_record("scheduled", 1))?;
        std::fs::write(&target, claude_record("must-not-import", 99))?;
        let source = SourceSpec {
            path: source_path.clone(),
            trusted_root: dir.path().to_path_buf(),
            client: Client::ClaudeCode,
            compressed: false,
        };
        let limits = DEFAULT_SCAN_LIMITS;
        let reserved = estimate_source_work(&source, limits)?;
        std::fs::remove_file(&source_path)?;
        if !try_create_test_file_symlink(&target, &source_path)? {
            return Ok(());
        }
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let adapter = StaticAdapter {
            sources: vec![source.clone()],
        };
        let mut remaining_work = limits.max_total_work_bytes - reserved;
        let mut work_reservation = SourceWorkReservation {
            reserved_bytes: reserved,
            remaining_bytes: &mut remaining_work,
        };

        assert!(
            scan_one(
                &mut ledger,
                &adapter,
                &source,
                &ScanOptions {
                    dry_run: true,
                    ..Default::default()
                },
                None,
                limits,
                &mut work_reservation,
            )
            .is_err()
        );
        assert!(ledger.canonical_events(None, None)?.is_empty());
        assert!(ledger.source_checkpoint(&source_path)?.is_none());
        assert!(ledger.source_checkpoint(&target)?.is_none());
        Ok(())
    }

    #[test]
    fn client_scoped_scan_keeps_global_snapshot_provisional() -> Result<()> {
        let dir = tempdir()?;
        let claude = dir.path().join("claude-scoped.jsonl");
        let codex = dir.path().join("codex-scoped.jsonl");
        std::fs::write(&claude, "{}\n")?;
        std::fs::write(&codex, "{}\n")?;
        let adapters = vec![
            client_static_adapter(Client::ClaudeCode, claude),
            client_static_adapter(Client::OpenaiCodex, codex.clone()),
        ];
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        let initial = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert!(!initial.provisional);
        let original_codex_size = ledger
            .source_checkpoint(&codex)?
            .context("Codex checkpoint missing")?
            .file_size;
        OpenOptions::new()
            .append(true)
            .open(&codex)?
            .write_all(b"{}\n")?;

        let scoped = ScanOptions {
            clients: HashSet::from([Client::ClaudeCode]),
            ..Default::default()
        };
        let summary = scan(&mut ledger, &Config::default(), &adapters, &scoped)?;
        assert!(summary.coverage_limited);
        assert!(summary.provisional);
        assert_eq!(summary.active_or_volatile_source_count, 0);
        assert_eq!(
            ledger
                .source_checkpoint(&codex)?
                .context("Codex checkpoint missing after scoped scan")?
                .file_size,
            original_codex_size
        );
        assert_eq!(
            ledger
                .coverage_snapshot()?
                .last_scan
                .as_ref()
                .map(|scan| scan.status.as_str()),
            Some("partial")
        );
        assert!(
            ledger
                .warning_code_counts()?
                .iter()
                .any(|warning| warning.code == "scan_client_scope_limited")
        );
        Ok(())
    }

    #[test]
    fn aggregate_work_limit_is_partial_without_mislabeling_sources_active() -> Result<()> {
        let dir = tempdir()?;
        let source = dir.path().join("bounded.jsonl");
        std::fs::write(&source, claude_record("bounded", 1))?;
        let adapters = static_adapters(source, false);
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;
        let mut limits = DEFAULT_SCAN_LIMITS;
        limits.max_total_work_bytes = 1;

        let summary = scan_with_limits(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
            limits,
        )?;
        assert_eq!(summary.scanned_sources, 0);
        assert_eq!(summary.active_or_volatile_source_count, 0);
        assert_eq!(summary.incomplete_source_count, 0);
        assert!(summary.coverage_limited);
        assert!(summary.provisional);
        assert!(
            ledger
                .warning_code_counts()?
                .iter()
                .any(|warning| warning.code == "scan_resource_limit")
        );
        Ok(())
    }

    #[test]
    fn full_digest_invalidates_large_same_size_mtime_preserving_rewrite() -> Result<()> {
        let dir = tempdir()?;
        let source = dir.path().join("large.jsonl");
        let padding_line = serde_json::json!({
            "type": "user",
            "padding": "x".repeat(4096)
        })
        .to_string()
            + "\n";
        // Keep the usage record well away from the former 4 KiB sample at the
        // head and from the next evenly-distributed sample window.
        let prefix = padding_line.repeat(32);
        let old_record = claude_record("large-rewrite", 1);
        let new_record = claude_record("large-rewrite", 9);
        assert_eq!(old_record.len(), new_record.len());
        let minimum_suffix = EXACT_FINGERPRINT_BYTES as usize + 1 - prefix.len() - old_record.len();
        let suffix = padding_line.repeat(minimum_suffix.div_ceil(padding_line.len()));
        let payload = format!("{prefix}{old_record}{suffix}");
        assert!(payload.len() as u64 > EXACT_FINGERPRINT_BYTES);
        std::fs::write(&source, &payload)?;
        let adapters = static_adapters(source.clone(), false);
        let mut ledger = Ledger::open(&dir.path().join("ledger.sqlite"))?;

        let initial = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(initial.scanned_sources, 1);
        assert!(!initial.provisional);
        assert_eq!(
            ledger.canonical_events(None, None)?[0]
                .usage
                .output_tokens_total,
            1
        );

        let original_modified = std::fs::metadata(&source)?.modified()?;
        let rewritten = payload.replacen(&old_record, &new_record, 1);
        assert_eq!(payload.len(), rewritten.len());
        std::fs::write(&source, rewritten)?;
        File::options()
            .write(true)
            .open(&source)?
            .set_modified(original_modified)?;

        let rescanned = scan(
            &mut ledger,
            &Config::default(),
            &adapters,
            &ScanOptions::default(),
        )?;
        assert_eq!(rescanned.scanned_sources, 1);
        assert_eq!(rescanned.reset_sources, 1);
        assert!(!rescanned.provisional);
        assert_eq!(
            ledger.canonical_events(None, None)?[0]
                .usage
                .output_tokens_total,
            9
        );
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
        let limited_summary = scan(&mut ledger, &config, &adapters, &limited)?;
        assert!(limited_summary.provisional);
        assert_eq!(limited_summary.active_or_volatile_source_count, 0);
        assert_eq!(limited_summary.incomplete_source_count, 1);
        assert!(limited_summary.coverage_limited);
        assert_eq!(
            ledger
                .coverage_snapshot()?
                .last_scan
                .as_ref()
                .map(|scan| scan.status.as_str()),
            Some("partial")
        );
        assert!(
            ledger
                .warning_code_counts()?
                .iter()
                .any(|warning| warning.code == "scan_limited")
        );
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
