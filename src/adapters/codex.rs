use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::adapters::{DiscoveryRequest, DiscoveryResult, SourceAdapter, discover_bounded_files};
use crate::config::{Config, PathOrigin, ResolvedPath};
use crate::model::{
    Client, CoverageStatus, DimensionValueProvenance, LineRecord, ParseBatch, PricingDimensions,
    ScanWarning, SourceSpec, UsageObservation, UsageQuality, UsageVector, pseudonymous_session_id,
    stable_id,
};

const PARSER_VERSION: &str = "codex-rollout-v1";
const ACTIVE_SESSIONS_DIR: &str = "sessions";
const ARCHIVED_SESSIONS_DIR: &str = "archived_sessions";

/// Parser for Codex rollout JSONL files.
///
/// Rollout payloads are a version-sensitive persistence format rather than a
/// public API. Consequently, this adapter only retains an allowlisted envelope:
/// session/model metadata, timestamps, and usage counters. Prompt, response,
/// reasoning, tool, and command bodies are never copied into parser state,
/// observations, or warnings.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

impl SourceAdapter for CodexAdapter {
    fn client(&self) -> Client {
        Client::OpenaiCodex
    }

    fn display_name(&self) -> &'static str {
        "OpenAI Codex"
    }

    fn discover_bounded(
        &self,
        config: &Config,
        request: DiscoveryRequest,
    ) -> Result<DiscoveryResult> {
        let (codex_home, explicit) = resolve_codex_home(config)?;
        if !codex_home
            .try_exists()
            .context("failed to inspect CODEX_HOME")?
        {
            if explicit {
                bail!("configured CODEX_HOME does not exist");
            }
            return Ok(DiscoveryResult::default());
        }
        if !fs::metadata(&codex_home)
            .context("failed to inspect CODEX_HOME metadata")?
            .is_dir()
        {
            bail!("configured CODEX_HOME is not a directory");
        }

        let codex_home = codex_home
            .canonicalize()
            .context("failed to canonicalize CODEX_HOME")?;
        let mut roots = Vec::new();
        for subdirectory in [ACTIVE_SESSIONS_DIR, ARCHIVED_SESSIONS_DIR] {
            let root = codex_home.join(subdirectory);
            if !root
                .try_exists()
                .with_context(|| format!("failed to inspect Codex {subdirectory} directory"))?
            {
                continue;
            }
            if !fs::metadata(&root)
                .with_context(|| format!("failed to inspect Codex {subdirectory} metadata"))?
                .is_dir()
            {
                bail!("Codex {subdirectory} path is not a directory");
            }
            roots.push(root);
        }

        let mut result = discover_bounded_files(&roots, request, |path| {
            let Some(compressed) = rollout_file_kind(path) else {
                return Ok(None);
            };
            // Current Codex may temporarily have both representations while
            // materializing an archived rollout. Prefer the plain file so one
            // physical rollout is not scanned twice.
            if compressed
                && let Some(plain) = plain_sibling(path)
                && plain
                    .try_exists()
                    .with_context(|| "failed to inspect plain sibling for a Codex rollout")?
            {
                return Ok(None);
            }
            Ok(Some(SourceSpec {
                path: path.to_path_buf(),
                trusted_root: roots
                    .iter()
                    .find(|root| path.starts_with(root))
                    .cloned()
                    .context("discovered Codex source escaped its trusted root")?,
                client: Client::OpenaiCodex,
                compressed,
            }))
        })?;
        // Rollout names begin with an ISO-like creation timestamp. Preserve a
        // stable order inside the selected bounded candidate page.
        result.sources.sort_by(|left, right| {
            rollout_sort_key(&left.path)
                .cmp(&rollout_sort_key(&right.path))
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(result)
    }

    fn parse_lines(
        &self,
        path: &Path,
        lines: &[LineRecord],
        previous_state: Option<&Value>,
    ) -> Result<ParseBatch> {
        let mut warnings = Vec::new();
        let mut state = match previous_state.filter(|value| !value.is_null()) {
            Some(value) => match serde_json::from_value::<CodexState>(value.clone()) {
                Ok(state) => state,
                Err(_) => {
                    warnings.push(ScanWarning::new(
                        "codex_state_reset",
                        "The saved Codex parser state was incompatible; this source was restarted safely.",
                    ));
                    CodexState::default()
                }
            },
            None => CodexState::default(),
        };
        state.normalize_session_ids();

        if state.physical_thread_id.is_none() {
            state.physical_thread_id = thread_id_from_rollout_name(path)
                .map(|value| pseudonymous_session_id(Client::OpenaiCodex, &value));
        }
        if (state.auth_mode.is_none() || state.auth_mode_inferred)
            && let Some(auth_mode) = current_profile_auth_mode(path)
        {
            state.auth_mode = Some(auth_mode);
            state.auth_mode_inferred = true;
        }

        let mut observations = Vec::new();
        for line in lines {
            if line.text.trim().is_empty() {
                warn_once(
                    &mut state,
                    &mut warnings,
                    "blank_record",
                    ScanWarning::new("codex_blank_record", "A blank rollout record was skipped.")
                        .at(line.locator()),
                );
                continue;
            }

            let value: Value = match serde_json::from_str(&line.text) {
                Ok(value) => value,
                Err(error) => {
                    // Do not include the line or serde error text: either could
                    // expose transcript content. The error category and locator
                    // are sufficient for a version/health diagnostic.
                    warnings.push(
                        ScanWarning::new(
                            "codex_malformed_record",
                            format!(
                                "A Codex rollout record with a {} JSON error was skipped safely.",
                                json_error_category(&error)
                            ),
                        )
                        .at(line.locator()),
                    );
                    continue;
                }
            };
            let Some(record) = value.as_object() else {
                warnings.push(
                    ScanWarning::new(
                        "codex_non_object_record",
                        "A non-object Codex rollout record was skipped safely.",
                    )
                    .at(line.locator()),
                );
                continue;
            };
            let Some(record_type) = record.get("type").and_then(Value::as_str) else {
                warnings.push(
                    ScanWarning::new(
                        "codex_missing_record_type",
                        "A Codex rollout record without a type was skipped safely.",
                    )
                    .at(line.locator()),
                );
                continue;
            };

            match record_type {
                "session_meta" => parse_session_meta(record, path, line, &mut state, &mut warnings),
                "turn_context" => parse_turn_context(record, line, &mut state, &mut warnings),
                "event_msg" => parse_event_message(
                    record,
                    path,
                    line,
                    &mut state,
                    &mut observations,
                    &mut warnings,
                ),
                // Known content-bearing or accounting-irrelevant records. They
                // are deliberately not traversed beyond their type envelope.
                "response_item"
                | "compacted"
                | "world_state"
                | "inter_agent_communication"
                | "inter_agent_communication_metadata" => {}
                unknown => {
                    let type_fingerprint = stable_id(&["codex-unknown-record-type", unknown]);
                    warn_once(
                        &mut state,
                        &mut warnings,
                        &format!("record_type:{type_fingerprint}"),
                        ScanWarning::new(
                            "codex_unknown_record_type",
                            format!(
                                "An unknown Codex rollout record type was skipped (type fingerprint {}); the local format may have changed.",
                                &type_fingerprint[..16]
                            ),
                        )
                        .at(line.locator()),
                    )
                }
            }
        }

        let incomplete = !warnings.is_empty();
        Ok(ParseBatch {
            observations,
            warnings,
            next_state: serde_json::to_value(state)?,
            incomplete,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct CodexState {
    canonical_meta_seen: bool,
    /// True only for state produced by this parser or sanitized by the ledger.
    /// Provider input never controls this state object.
    session_ids_private: bool,
    logical_session_id: Option<String>,
    physical_thread_id: Option<String>,
    client_version: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    service_tier: Option<String>,
    auth_mode: Option<String>,
    auth_mode_inferred: bool,
    context_window: Option<u64>,
    epoch: u64,
    usage_event_index: u64,
    previous: Option<RawUsage>,
    warned: BTreeSet<String>,
}

impl CodexState {
    fn normalize_session_ids(&mut self) {
        let trusted = self.session_ids_private;
        self.logical_session_id = self
            .logical_session_id
            .take()
            .map(|value| normalize_state_session_id(&value, trusted));
        self.physical_thread_id = self
            .physical_thread_id
            .take()
            .map(|value| normalize_state_session_id(&value, trusted));
        self.session_ids_private = true;
    }
}

fn normalize_state_session_id(value: &str, trusted: bool) -> String {
    let trusted_private = trusted
        && value.strip_prefix("tlses_").is_some_and(|digest| {
            digest.len() == 24 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    if trusted_private {
        value.to_ascii_lowercase()
    } else {
        pseudonymous_session_id(Client::OpenaiCodex, value)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct RawUsage {
    input: u64,
    cached_input: u64,
    output: u64,
    reasoning_output: u64,
    total: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    cache_write_unknown: u64,
    cached_input_reported: bool,
    reasoning_output_reported: bool,
    cache_write_reported: bool,
}

impl RawUsage {
    fn same_counters(&self, other: &Self) -> bool {
        self.input == other.input
            && self.cached_input == other.cached_input
            && self.output == other.output
            && self.reasoning_output == other.reasoning_output
            && self.total == other.total
            && self.cache_write_5m == other.cache_write_5m
            && self.cache_write_1h == other.cache_write_1h
            && self.cache_write_unknown == other.cache_write_unknown
    }

    fn decreased_from(&self, previous: &Self) -> bool {
        self.input < previous.input
            || self.cached_input < previous.cached_input
            || self.output < previous.output
            || self.reasoning_output < previous.reasoning_output
            || self.total < previous.total
            || (self.cache_write_reported
                && previous.cache_write_reported
                && (self.cache_write_5m < previous.cache_write_5m
                    || self.cache_write_1h < previous.cache_write_1h
                    || self.cache_write_unknown < previous.cache_write_unknown))
    }

    fn boundary(&self) -> String {
        format!(
            "i={};ci={};o={};ro={};t={};w5={};w1={};wu={}",
            self.input,
            self.cached_input,
            self.output,
            self.reasoning_output,
            self.total,
            self.cache_write_5m,
            self.cache_write_1h,
            self.cache_write_unknown
        )
    }
}

#[derive(Debug, Clone)]
struct UsageDelta {
    input: u64,
    cached_input: u64,
    output: u64,
    reasoning_output: u64,
    total: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    cache_write_unknown: u64,
    cached_input_complete: bool,
    reasoning_output_complete: bool,
    cache_write_complete: bool,
}

impl UsageDelta {
    fn from_boundary(current: &RawUsage, previous: Option<&RawUsage>) -> Self {
        let subtract = |current: u64, previous: u64| current.saturating_sub(previous);
        match previous {
            None => Self {
                input: current.input,
                cached_input: current.cached_input,
                output: current.output,
                reasoning_output: current.reasoning_output,
                total: current.total,
                cache_write_5m: current.cache_write_5m,
                cache_write_1h: current.cache_write_1h,
                cache_write_unknown: current.cache_write_unknown,
                cached_input_complete: current.cached_input_reported,
                reasoning_output_complete: current.reasoning_output_reported,
                cache_write_complete: current.cache_write_reported,
            },
            Some(previous) => {
                let cache_write_complete =
                    current.cache_write_reported && previous.cache_write_reported;
                Self {
                    input: subtract(current.input, previous.input),
                    cached_input: if current.cached_input_reported && previous.cached_input_reported
                    {
                        subtract(current.cached_input, previous.cached_input)
                    } else {
                        0
                    },
                    output: subtract(current.output, previous.output),
                    reasoning_output: if current.reasoning_output_reported
                        && previous.reasoning_output_reported
                    {
                        subtract(current.reasoning_output, previous.reasoning_output)
                    } else {
                        0
                    },
                    total: subtract(current.total, previous.total),
                    cache_write_5m: if cache_write_complete {
                        subtract(current.cache_write_5m, previous.cache_write_5m)
                    } else {
                        0
                    },
                    cache_write_1h: if cache_write_complete {
                        subtract(current.cache_write_1h, previous.cache_write_1h)
                    } else {
                        0
                    },
                    cache_write_unknown: if cache_write_complete {
                        subtract(current.cache_write_unknown, previous.cache_write_unknown)
                    } else {
                        0
                    },
                    cached_input_complete: current.cached_input_reported
                        && previous.cached_input_reported,
                    reasoning_output_complete: current.reasoning_output_reported
                        && previous.reasoning_output_reported,
                    cache_write_complete,
                }
            }
        }
    }

    fn has_billable_components(&self) -> bool {
        self.input != 0
            || self.output != 0
            || self.cached_input != 0
            || self.cache_write_5m != 0
            || self.cache_write_1h != 0
            || self.cache_write_unknown != 0
    }
}

impl CodexAdapter {
    pub fn resolve_home_with_origin(config: &Config) -> Result<ResolvedPath> {
        let home = BaseDirs::new()
            .context("could not determine the user home directory for default CODEX_HOME")?;
        resolve_home_candidates(
            config.codex_home_override.as_deref(),
            env::var_os("CODEX_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            config.codex_home.as_deref(),
            home.home_dir().join(".codex"),
        )
    }
}

fn resolve_codex_home(config: &Config) -> Result<(PathBuf, bool)> {
    let resolved = CodexAdapter::resolve_home_with_origin(config)?;
    Ok((resolved.path, resolved.origin != PathOrigin::Default))
}

fn resolve_home_candidates(
    cli: Option<&Path>,
    environment: Option<PathBuf>,
    configured: Option<&Path>,
    default: PathBuf,
) -> Result<ResolvedPath> {
    let (path, origin) = if let Some(path) = cli {
        (path.to_path_buf(), PathOrigin::Cli)
    } else if let Some(path) = environment {
        (path, PathOrigin::Environment)
    } else if let Some(path) = configured {
        (path.to_path_buf(), PathOrigin::Config)
    } else {
        (default, PathOrigin::Default)
    };
    let path = if let Ok(remainder) = path.strip_prefix("~") {
        BaseDirs::new()
            .context("could not expand '~' without a home directory")?
            .home_dir()
            .join(remainder)
    } else {
        path
    };
    Ok(ResolvedPath { path, origin })
}

fn rollout_file_kind(path: &Path) -> Option<bool> {
    let name = path.file_name()?.to_str()?;
    if !name.starts_with("rollout-") {
        return None;
    }
    if name.ends_with(".jsonl.zst") {
        Some(true)
    } else if name.ends_with(".jsonl") {
        Some(false)
    } else {
        None
    }
}

fn plain_sibling(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let plain_name = name.strip_suffix(".zst")?;
    Some(path.with_file_name(plain_name))
}

fn rollout_sort_key(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .trim_end_matches(".zst")
        .to_string()
}

fn thread_id_from_rollout_name(path: &Path) -> Option<String> {
    let mut name = path.file_name()?.to_str()?;
    name = name.strip_suffix(".zst").unwrap_or(name);
    name = name.strip_suffix(".jsonl")?;
    if name.len() < 36 {
        return None;
    }
    let candidate = &name[name.len() - 36..];
    let valid = candidate.char_indices().all(|(index, character)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            character == '-'
        } else {
            character.is_ascii_hexdigit()
        }
    });
    valid.then(|| candidate.to_ascii_lowercase())
}

fn parse_session_meta(
    record: &Map<String, Value>,
    path: &Path,
    line: &LineRecord,
    state: &mut CodexState,
    warnings: &mut Vec<ScanWarning>,
) {
    let Some(payload) = record.get("payload").and_then(Value::as_object) else {
        warnings.push(
            ScanWarning::new(
                "codex_invalid_session_meta",
                "A Codex session_meta record without an object payload was skipped.",
            )
            .at(line.locator()),
        );
        return;
    };

    if state.canonical_meta_seen {
        // Forked rollouts intentionally copy older SessionMeta records. The
        // first SessionMeta belongs to the physical file; later ones are
        // history and must not replace its identity/provider provenance.
        return;
    }

    let record_thread_id = string_field(payload, &["id", "thread_id"])
        .map(|value| pseudonymous_session_id(Client::OpenaiCodex, &value.to_ascii_lowercase()));
    if let (Some(expected), Some(found)) = (
        state.physical_thread_id.as_deref(),
        record_thread_id.as_deref(),
    ) && !expected.eq_ignore_ascii_case(found)
    {
        warn_once(
            state,
            warnings,
            "canonical_meta_missing",
            ScanWarning::new(
                "codex_primary_session_meta_missing",
                "A copied session_meta record appeared before this rollout's primary metadata; file identity fallback remains active.",
            )
            .at(line.locator()),
        );
        return;
    }

    state.canonical_meta_seen = true;
    if state.physical_thread_id.is_none() {
        state.physical_thread_id = record_thread_id.clone();
    }
    state.logical_session_id = string_field(payload, &["session_id"])
        .map(|value| pseudonymous_session_id(Client::OpenaiCodex, &value))
        .or(record_thread_id)
        .or_else(|| {
            thread_id_from_rollout_name(path)
                .map(|value| pseudonymous_session_id(Client::OpenaiCodex, &value))
        });
    state.client_version = string_field(payload, &["cli_version"]);
    state.provider = string_field(payload, &["model_provider", "model_provider_id"]);
    if let Some(auth_mode) = string_field(payload, &["auth_mode"]) {
        state.auth_mode = Some(auth_mode);
        state.auth_mode_inferred = false;
    }

    if state.client_version.is_none() {
        warn_once(
            state,
            warnings,
            "missing_client_version",
            ScanWarning::new(
                "codex_missing_client_version",
                "Codex client-version provenance was unavailable; format compatibility is best effort.",
            )
            .at(line.locator()),
        );
    }
}

fn parse_turn_context(
    record: &Map<String, Value>,
    line: &LineRecord,
    state: &mut CodexState,
    warnings: &mut Vec<ScanWarning>,
) {
    let Some(payload) = record.get("payload").and_then(Value::as_object) else {
        warnings.push(
            ScanWarning::new(
                "codex_invalid_turn_context",
                "A Codex turn_context record without an object payload was skipped.",
            )
            .at(line.locator()),
        );
        return;
    };
    apply_context(payload, state);
}

fn apply_context(payload: &Map<String, Value>, state: &mut CodexState) {
    if let Some(model) = string_field(payload, &["model"]) {
        state.model = Some(model);
    }
    if let Some(provider) = string_field(payload, &["model_provider", "model_provider_id"]) {
        state.provider = Some(provider);
    }
    if let Some(value) = payload.get("service_tier") {
        state.service_tier = value.as_str().map(ToOwned::to_owned);
    }
    if let Some(value) = payload.get("auth_mode") {
        state.auth_mode = value.as_str().map(ToOwned::to_owned);
        state.auth_mode_inferred = false;
    }
}

#[derive(Debug, Deserialize)]
struct CodexAuthEnvelope {
    auth_mode: Option<String>,
}

/// Read only the non-secret authentication mode from the current Codex
/// profile. Rollouts do not consistently persist this pricing dimension, while
/// `auth.json` also contains credentials that must never enter parser state.
fn current_profile_auth_mode(path: &Path) -> Option<String> {
    let sessions_root = path.ancestors().find(|ancestor| {
        ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.eq_ignore_ascii_case(ACTIVE_SESSIONS_DIR)
                    || name.eq_ignore_ascii_case(ARCHIVED_SESSIONS_DIR)
            })
    })?;
    let auth_path = sessions_root.parent()?.join("auth.json");
    let file = std::fs::File::open(auth_path).ok()?;
    let envelope: CodexAuthEnvelope = serde_json::from_reader(file).ok()?;
    envelope.auth_mode.filter(|value| !value.trim().is_empty())
}

fn speed_from_service_tier(service_tier: Option<&str>) -> Option<String> {
    match service_tier?.trim().to_ascii_lowercase().as_str() {
        "fast" => Some("fast".to_string()),
        "default" | "standard" | "normal" => Some("standard".to_string()),
        _ => None,
    }
}

fn parse_event_message(
    record: &Map<String, Value>,
    path: &Path,
    line: &LineRecord,
    state: &mut CodexState,
    observations: &mut Vec<UsageObservation>,
    warnings: &mut Vec<ScanWarning>,
) {
    let Some(payload) = record.get("payload").and_then(Value::as_object) else {
        warnings.push(
            ScanWarning::new(
                "codex_invalid_event_message",
                "A Codex event_msg record without an object payload was skipped.",
            )
            .at(line.locator()),
        );
        return;
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("token_count") => {
            parse_token_count(record, payload, path, line, state, observations, warnings)
        }
        Some("thread_settings_applied") => {
            if let Some(settings) = payload.get("thread_settings").and_then(Value::as_object) {
                apply_context(settings, state);
            } else {
                warnings.push(
                    ScanWarning::new(
                        "codex_invalid_thread_settings",
                        "A thread_settings_applied event without settings was skipped.",
                    )
                    .at(line.locator()),
                );
            }
        }
        // Other event messages may contain transcript/tool material. Their
        // type is known to be extensible, so ignoring them is not drift.
        Some(_) => {}
        None => warnings.push(
            ScanWarning::new(
                "codex_missing_event_type",
                "A Codex event_msg record without an event type was skipped.",
            )
            .at(line.locator()),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_token_count(
    record: &Map<String, Value>,
    payload: &Map<String, Value>,
    path: &Path,
    line: &LineRecord,
    state: &mut CodexState,
    observations: &mut Vec<UsageObservation>,
    warnings: &mut Vec<ScanWarning>,
) {
    let Some(info_value) = payload.get("info") else {
        // `info` is optional in the upstream protocol; a rate-limit-only event
        // is not a malformed usage record.
        return;
    };
    if info_value.is_null() {
        return;
    }
    let Some(info) = info_value.as_object() else {
        warnings.push(
            ScanWarning::new(
                "codex_invalid_token_info",
                "A token_count event with non-object usage info was skipped.",
            )
            .at(line.locator()),
        );
        return;
    };

    warn_unknown_keys(
        info,
        &[
            "total_token_usage",
            "last_token_usage",
            "model_context_window",
        ],
        "token_info",
        line,
        state,
        warnings,
    );

    if let Some(context_window) = info.get("model_context_window") {
        if context_window.is_null() {
            state.context_window = None;
        } else if let Some(value) = context_window.as_u64() {
            state.context_window = Some(value);
        } else {
            warn_once(
                state,
                warnings,
                "invalid_context_window",
                ScanWarning::new(
                    "codex_invalid_context_window",
                    "A non-integer model_context_window was ignored.",
                )
                .at(line.locator()),
            );
        }
    }

    let Some(total_value) = info.get("total_token_usage") else {
        warnings.push(
            ScanWarning::new(
                "codex_missing_cumulative_usage",
                "A token_count event without total_token_usage was skipped; cumulative totals are required.",
            )
            .at(line.locator()),
        );
        return;
    };
    let Some(total_object) = total_value.as_object() else {
        warnings.push(
            ScanWarning::new(
                "codex_invalid_cumulative_usage",
                "A token_count event with non-object total_token_usage was skipped.",
            )
            .at(line.locator()),
        );
        return;
    };

    warn_unknown_usage_keys(total_object, line, state, warnings);
    let current = match parse_raw_usage(total_object) {
        Ok(usage) => usage,
        Err(issue) => {
            warnings.push(
                ScanWarning::new(
                    "codex_unsupported_usage_schema",
                    format!(
                        "A token_count event was skipped because counter field '{}' was {}.",
                        issue.field, issue.problem
                    ),
                )
                .at(line.locator()),
            );
            return;
        }
    };

    let prior = state.previous.as_ref();
    if prior.is_some_and(|previous| current.same_counters(previous)) {
        // Update capability flags even when a newer client begins reporting an
        // all-zero optional dimension at the same cumulative boundary.
        state.previous = Some(current);
        return;
    }

    let reset = prior.is_some_and(|previous| current.decreased_from(previous));
    let proposed_epoch = state.epoch.saturating_add(u64::from(reset));
    let delta = UsageDelta::from_boundary(&current, if reset { None } else { prior });

    if reset {
        warnings.push(
            ScanWarning::new(
                "codex_counter_reset",
                "Codex cumulative counters decreased; a new non-negative accumulator epoch was started.",
            )
            .at(line.locator()),
        );
    }

    // Codex uses component-zero snapshots with total_tokens set to a context
    // window after compaction. They establish an epoch baseline but are not
    // billable usage and must not become zero-cost observations.
    if !delta.has_billable_components() {
        state.epoch = proposed_epoch;
        state.previous = Some(current);
        return;
    }

    let occurred_at = match parse_record_timestamp(record) {
        Ok(timestamp) => timestamp,
        Err(message) => {
            warnings.push(
                ScanWarning::new("codex_invalid_usage_timestamp", message).at(line.locator()),
            );
            // Leave the previous boundary untouched. A later valid cumulative
            // event can still recover these tokens instead of silently losing
            // them from the ledger.
            return;
        }
    };

    let session_id = state
        .logical_session_id
        .clone()
        .or_else(|| state.physical_thread_id.clone())
        .unwrap_or_else(|| pseudonymous_session_id(Client::OpenaiCodex, &rollout_sort_key(path)));
    if !state.canonical_meta_seen {
        warn_once(
            state,
            warnings,
            "missing_primary_meta_for_usage",
            ScanWarning::new(
                "codex_usage_without_session_meta",
                "Usage was recovered with a rollout-name session fallback because primary session metadata was unavailable.",
            )
            .at(line.locator()),
        );
    }

    let model = state.model.clone().unwrap_or_else(|| "unknown".to_string());
    let provider = state
        .provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    if state.model.is_none() {
        warn_once(
            state,
            warnings,
            "usage_without_model",
            ScanWarning::new(
                "codex_usage_without_model_context",
                "Usage could not be associated with a turn/model context and will remain unpriced until resolved.",
            )
            .at(line.locator()),
        );
    }

    let cache_write = delta
        .cache_write_5m
        .saturating_add(delta.cache_write_1h)
        .saturating_add(delta.cache_write_unknown);
    let known_input_subsets = delta.cached_input.saturating_add(cache_write);
    let subsets_valid = known_input_subsets <= delta.input;
    let reasoning_valid = delta.reasoning_output <= delta.output;
    let total_consistent = delta.total == delta.input.saturating_add(delta.output);

    let mut observation_warnings = Vec::new();
    if !delta.cached_input_complete {
        observation_warnings.push("cached_input_not_fully_reported".to_string());
    }
    if !delta.cache_write_complete {
        observation_warnings.push("cache_write_not_reported".to_string());
    }
    if !delta.reasoning_output_complete {
        observation_warnings.push("reasoning_output_not_fully_reported".to_string());
    }
    if !subsets_valid {
        observation_warnings.push("input_subsets_exceed_total_input".to_string());
        warnings.push(
            ScanWarning::new(
                "codex_invalid_input_subsets",
                "Cached/write input deltas exceeded total input; uncached input was clamped to zero.",
            )
            .at(line.locator()),
        );
    }
    if !reasoning_valid {
        observation_warnings.push("reasoning_output_exceeds_output".to_string());
        warnings.push(
            ScanWarning::new(
                "codex_invalid_reasoning_subset",
                "Reasoning output exceeded inclusive output; output was retained without double counting.",
            )
            .at(line.locator()),
        );
    }
    if !total_consistent {
        observation_warnings.push("total_tokens_not_component_additive".to_string());
    }
    if state.auth_mode_inferred {
        observation_warnings.push(
            "auth mode was inferred from the current Codex profile; a historical session may have used a different route"
                .to_string(),
        );
    }

    let coverage_complete = delta.cached_input_complete
        && delta.cache_write_complete
        && delta.reasoning_output_complete
        && subsets_valid
        && reasoning_valid
        && !state.auth_mode_inferred;
    let usage = UsageVector {
        input_tokens_total: delta.input,
        input_tokens_uncached: delta.input.saturating_sub(known_input_subsets),
        input_tokens_cached: delta.cached_input,
        cache_write_5m_tokens: delta.cache_write_5m,
        cache_write_1h_tokens: delta.cache_write_1h,
        cache_write_unknown_tokens: delta.cache_write_unknown,
        output_tokens_total: delta.output,
        reasoning_output_tokens: delta.reasoning_output,
        ..Default::default()
    };

    let epoch = proposed_epoch.to_string();
    let boundary = current.boundary();
    let event_key = stable_id(&["codex-counter-boundary", &session_id, &epoch, &boundary]);
    state.usage_event_index = state.usage_event_index.saturating_add(1);
    let usage_event_index = state.usage_event_index;
    let parser_version = match state.client_version.as_deref() {
        Some(version) => format!("{PARSER_VERSION};codex-cli={version}"),
        None => format!("{PARSER_VERSION};codex-cli=unknown"),
    };
    observations.push(UsageObservation {
        event_key,
        client: Client::OpenaiCodex,
        session_id,
        usage_event_index: Some(usage_event_index),
        provider_message_id: None,
        occurred_at,
        raw_model: model,
        provider,
        usage,
        dimensions: PricingDimensions {
            auth_mode: state.auth_mode.clone(),
            auth_mode_provenance: state.auth_mode.as_ref().map(|_| {
                if state.auth_mode_inferred {
                    DimensionValueProvenance::CurrentProfileInferred
                } else {
                    DimensionValueProvenance::SourceObserved
                }
            }),
            provider_route: None,
            provider_request_id: None,
            service_tier: state.service_tier.clone(),
            service_tier_provenance: state
                .service_tier
                .as_ref()
                .map(|_| DimensionValueProvenance::SourceObserved),
            speed: speed_from_service_tier(state.service_tier.as_deref()),
            speed_provenance: speed_from_service_tier(state.service_tier.as_deref())
                .map(|_| DimensionValueProvenance::SourceObserved),
            inference_geo: None,
            context_window: state.context_window,
            cache_write_data_complete: Some(delta.cache_write_complete),
            input_subset_accounting_consistent: Some(subsets_valid),
            ..PricingDimensions::default()
        },
        quality: UsageQuality::Derived,
        coverage: if coverage_complete {
            CoverageStatus::CompleteKnown
        } else {
            CoverageStatus::PartialKnown
        },
        source_locator: line.locator(),
        parser_version,
        warnings: observation_warnings,
    });

    state.epoch = proposed_epoch;
    state.previous = Some(current);
}

#[derive(Debug)]
struct CounterIssue {
    field: &'static str,
    problem: &'static str,
}

fn parse_raw_usage(object: &Map<String, Value>) -> std::result::Result<RawUsage, CounterIssue> {
    let input = required_counter(object, "input_tokens")?;
    let output = required_counter(object, "output_tokens")?;
    let cached = optional_counter(object, "cached_input_tokens")?;
    let reasoning = optional_counter(object, "reasoning_output_tokens")?;
    let total = match optional_counter(object, "total_tokens")? {
        Some(total) => total,
        None => input.saturating_add(output),
    };

    let cache_write_5m = first_optional_counter(
        object,
        &["cache_write_5m_tokens", "cache_creation_5m_input_tokens"],
    )?;
    let cache_write_1h = first_optional_counter(
        object,
        &["cache_write_1h_tokens", "cache_creation_1h_input_tokens"],
    )?;
    let generic_cache_write = first_optional_counter(
        object,
        &[
            "cache_write_tokens",
            "cache_write_input_tokens",
            "cache_creation_input_tokens",
        ],
    )?;
    let specific_cache_write_reported = cache_write_5m.is_some() || cache_write_1h.is_some();
    let cache_write_reported = specific_cache_write_reported || generic_cache_write.is_some();

    Ok(RawUsage {
        input,
        cached_input: cached.unwrap_or_default(),
        output,
        reasoning_output: reasoning.unwrap_or_default(),
        total,
        cache_write_5m: cache_write_5m.unwrap_or_default(),
        cache_write_1h: cache_write_1h.unwrap_or_default(),
        // When TTL-specific counters exist, a generic field is assumed to be
        // their aggregate/alias and is not added a second time.
        cache_write_unknown: if specific_cache_write_reported {
            0
        } else {
            generic_cache_write.unwrap_or_default()
        },
        cached_input_reported: cached.is_some(),
        reasoning_output_reported: reasoning.is_some(),
        cache_write_reported,
    })
}

fn required_counter(
    object: &Map<String, Value>,
    field: &'static str,
) -> std::result::Result<u64, CounterIssue> {
    match object.get(field) {
        None => Err(CounterIssue {
            field,
            problem: "missing",
        }),
        Some(value) => value.as_u64().ok_or(CounterIssue {
            field,
            problem: "negative or non-integer",
        }),
    }
}

fn optional_counter(
    object: &Map<String, Value>,
    field: &'static str,
) -> std::result::Result<Option<u64>, CounterIssue> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(CounterIssue {
            field,
            problem: "negative or non-integer",
        }),
    }
}

fn first_optional_counter(
    object: &Map<String, Value>,
    fields: &[&'static str],
) -> std::result::Result<Option<u64>, CounterIssue> {
    for field in fields {
        if object.contains_key(*field) {
            return optional_counter(object, field);
        }
    }
    Ok(None)
}

fn warn_unknown_usage_keys(
    object: &Map<String, Value>,
    line: &LineRecord,
    state: &mut CodexState,
    warnings: &mut Vec<ScanWarning>,
) {
    const KNOWN: &[&str] = &[
        "input_tokens",
        "cached_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
        "cache_write_tokens",
        "cache_write_input_tokens",
        "cache_creation_input_tokens",
        "cache_write_5m_tokens",
        "cache_creation_5m_input_tokens",
        "cache_write_1h_tokens",
        "cache_creation_1h_input_tokens",
    ];
    warn_unknown_keys(object, KNOWN, "total_token_usage", line, state, warnings);
}

fn warn_unknown_keys(
    object: &Map<String, Value>,
    known: &[&str],
    scope: &str,
    line: &LineRecord,
    state: &mut CodexState,
    warnings: &mut Vec<ScanWarning>,
) {
    let mut unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|field| !known.contains(field))
        .collect();
    unknown.sort_unstable();
    if unknown.is_empty() {
        return;
    }
    // Unknown field names are part of the source JSON and may be
    // attacker-controlled. Keep a stable fingerprint for diagnostics without
    // copying those raw fragments into warnings, stdout, or SQLite.
    let field_signature = stable_id(&["codex-unknown-fields", scope, &unknown.join("\u{1f}")]);
    let field_count = unknown.len();
    warn_once(
        state,
        warnings,
        &format!("unknown_keys:{scope}:{field_signature}"),
        ScanWarning::new(
            "codex_format_drift",
            format!(
                "{field_count} unrecognized {scope} field(s) were ignored (field fingerprint {}); verify compatibility with this Codex version.",
                &field_signature[..16]
            ),
        )
        .at(line.locator()),
    );
}

fn parse_record_timestamp(
    record: &Map<String, Value>,
) -> std::result::Result<DateTime<Utc>, String> {
    let Some(timestamp) = record.get("timestamp").and_then(Value::as_str) else {
        return Err("A usage-bearing token_count event had no timestamp and was deferred.".into());
    };
    DateTime::parse_from_rfc3339(timestamp)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| {
            "A usage-bearing token_count event had an invalid timestamp and was deferred.".into()
        })
}

fn string_field(object: &Map<String, Value>, fields: &[&str]) -> Option<String> {
    fields
        .iter()
        .find_map(|field| object.get(*field).and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn warn_once(
    state: &mut CodexState,
    warnings: &mut Vec<ScanWarning>,
    key: &str,
    warning: ScanWarning,
) {
    if state.warned.insert(key.to_string()) {
        warnings.push(warning);
    }
}

fn json_error_category(error: &serde_json::Error) -> &'static str {
    use serde_json::error::Category;
    match error.classify() {
        Category::Io => "I/O",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "incomplete-tail",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const ROOT_SESSION: &str = "aaaaaaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa";
    const PARENT_THREAD: &str = "11111111-1111-7111-8111-111111111111";
    const CHILD_THREAD: &str = "22222222-2222-7222-8222-222222222222";
    const SENSITIVE_SENTINEL: &str = "SENSITIVE_FIXTURE_PROMPT_MUST_NOT_ESCAPE";

    fn records(text: &str, starting_line: u64) -> Vec<LineRecord> {
        let mut offset = 0_u64;
        text.lines()
            .enumerate()
            .map(|(index, text)| {
                let start = offset;
                offset = offset.saturating_add(text.len() as u64).saturating_add(1);
                LineRecord {
                    line_number: starting_line + index as u64,
                    byte_start: start,
                    byte_end: offset,
                    text: text.to_string(),
                }
            })
            .collect()
    }

    fn jsonl(values: &[serde_json::Value]) -> String {
        let mut text = values
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
        text
    }

    fn rollout_path(thread_id: &str) -> PathBuf {
        PathBuf::from(format!(
            "/fixtures/rollout-2026-07-10T12-00-00-{thread_id}.jsonl"
        ))
    }

    #[test]
    fn home_candidates_follow_cli_environment_config_default_precedence() {
        let default = PathBuf::from("C:/default-codex");
        let configured = PathBuf::from("C:/configured-codex");
        let environment = PathBuf::from("C:/environment-codex");
        let cli = PathBuf::from("C:/cli-codex");

        let resolved = resolve_home_candidates(
            Some(&cli),
            Some(environment.clone()),
            Some(&configured),
            default.clone(),
        )
        .unwrap();
        assert_eq!(resolved.path, cli);
        assert_eq!(resolved.origin, PathOrigin::Cli);

        let resolved = resolve_home_candidates(
            None,
            Some(environment.clone()),
            Some(&configured),
            default.clone(),
        )
        .unwrap();
        assert_eq!(resolved.path, environment);
        assert_eq!(resolved.origin, PathOrigin::Environment);

        let resolved =
            resolve_home_candidates(None, None, Some(&configured), default.clone()).unwrap();
        assert_eq!(resolved.path, configured);
        assert_eq!(resolved.origin, PathOrigin::Config);

        let resolved = resolve_home_candidates(None, None, None, default.clone()).unwrap();
        assert_eq!(resolved.path, default);
        assert_eq!(resolved.origin, PathOrigin::Default);
    }

    #[test]
    fn parses_deltas_dedupes_repeats_and_tracks_model_switches() {
        let fixture = include_str!("fixtures/codex_basic.jsonl");
        let batch = CodexAdapter
            .parse_lines(&rollout_path(PARENT_THREAD), &records(fixture, 1), None)
            .expect("parse fixture");

        assert_eq!(batch.observations.len(), 3);
        let first = &batch.observations[0];
        assert_eq!(
            first.session_id,
            pseudonymous_session_id(Client::OpenaiCodex, ROOT_SESSION)
        );
        assert_eq!(first.raw_model, "gpt-alpha");
        assert_eq!(first.provider, "openai");
        assert_eq!(first.dimensions.service_tier.as_deref(), Some("priority"));
        assert_eq!(first.usage.input_tokens_total, 100);
        assert_eq!(first.usage.input_tokens_cached, 20);
        assert_eq!(first.usage.input_tokens_uncached, 80);
        assert_eq!(first.usage.output_tokens_total, 10);
        assert_eq!(first.usage.reasoning_output_tokens, 4);
        assert_eq!(first.quality, UsageQuality::Derived);
        assert_eq!(first.coverage, CoverageStatus::PartialKnown);
        assert_eq!(first.dimensions.context_window, Some(200_000));

        let second = &batch.observations[1];
        assert_eq!(second.usage.input_tokens_total, 60);
        assert_eq!(second.usage.input_tokens_cached, 30);
        assert_eq!(second.usage.input_tokens_uncached, 30);
        assert_eq!(second.usage.output_tokens_total, 15);
        assert_eq!(second.usage.reasoning_output_tokens, 3);

        let third = &batch.observations[2];
        assert_eq!(third.raw_model, "gpt-beta");
        assert_eq!(third.usage.input_tokens_total, 60);
        assert_eq!(third.usage.input_tokens_cached, 20);
        assert_eq!(third.usage.input_tokens_uncached, 40);
        assert_eq!(third.usage.output_tokens_total, 10);
        assert_eq!(third.usage.reasoning_output_tokens, 2);

        let serialized =
            serde_json::to_string(&batch.observations).expect("serialize observations");
        let warning_text = format!("{:?}", batch.warnings);
        assert!(!serialized.contains(SENSITIVE_SENTINEL));
        assert!(!warning_text.contains(SENSITIVE_SENTINEL));
    }

    #[test]
    fn infers_only_auth_mode_from_current_profile() {
        let temp = TempDir::new().expect("tempdir");
        let sessions = temp.path().join("sessions/2026/07/10");
        fs::create_dir_all(&sessions).unwrap();
        let secret = "AUTH_PROFILE_SECRET_MUST_NOT_ESCAPE";
        fs::write(
            temp.path().join("auth.json"),
            format!(
                r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":"{secret}","tokens":{{"access_token":"{secret}"}}}}"#
            ),
        )
        .unwrap();
        let path = sessions.join(format!("rollout-2026-07-10T12-00-00-{PARENT_THREAD}.jsonl"));

        let batch = CodexAdapter
            .parse_lines(
                &path,
                &records(include_str!("fixtures/codex_basic.jsonl"), 1),
                None,
            )
            .expect("profile inference");

        assert!(!batch.observations.is_empty());
        assert!(
            batch.observations.iter().all(|observation| {
                observation.dimensions.auth_mode.as_deref() == Some("chatgpt")
            })
        );
        assert!(batch.observations.iter().all(|observation| {
            observation.dimensions.auth_mode_provenance
                == Some(DimensionValueProvenance::CurrentProfileInferred)
        }));
        assert!(
            batch
                .observations
                .iter()
                .all(|observation| observation.coverage == CoverageStatus::PartialKnown)
        );
        assert!(
            !serde_json::to_string(&batch.observations)
                .unwrap()
                .contains(secret)
        );
        assert!(
            !serde_json::to_string(&batch.next_state)
                .unwrap()
                .contains(secret)
        );
    }

    #[test]
    fn preserves_state_across_incremental_batches() {
        let fixture = include_str!("fixtures/codex_basic.jsonl");
        let all = records(fixture, 1);
        let split = 5;
        let first = CodexAdapter
            .parse_lines(&rollout_path(PARENT_THREAD), &all[..split], None)
            .expect("first batch");
        let second = CodexAdapter
            .parse_lines(
                &rollout_path(PARENT_THREAD),
                &all[split..],
                Some(&first.next_state),
            )
            .expect("second batch");
        let persisted_state = first.next_state.clone();
        assert_eq!(
            persisted_state
                .get("session_ids_private")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(!persisted_state.to_string().contains(ROOT_SESSION));
        let resumed_from_private_state = CodexAdapter
            .parse_lines(
                &rollout_path(PARENT_THREAD),
                &all[split..],
                Some(&persisted_state),
            )
            .expect("second batch from persisted private state");

        assert_eq!(
            first.observations.len() + second.observations.len(),
            3,
            "repeat boundary must not be emitted across batches"
        );
        assert_eq!(
            second
                .observations
                .iter()
                .map(|observation| &observation.event_key)
                .collect::<Vec<_>>(),
            resumed_from_private_state
                .observations
                .iter()
                .map(|observation| &observation.event_key)
                .collect::<Vec<_>>(),
            "event identity must not change when persisted state pseudonymizes the session"
        );
        assert_eq!(second.observations.last().unwrap().raw_model, "gpt-beta");
    }

    #[test]
    fn crafted_session_pseudonym_lookalike_is_hashed_before_incremental_state() {
        const LOOKALIKE: &str = "tlses_0123456789abcdef01234567";

        let fixture = include_str!("fixtures/codex_basic.jsonl").replace(ROOT_SESSION, LOOKALIKE);
        let all = records(&fixture, 1);
        let split = 5;
        let first = CodexAdapter
            .parse_lines(&rollout_path(PARENT_THREAD), &all[..split], None)
            .expect("fresh prefix-lookalike batch");
        let second = CodexAdapter
            .parse_lines(
                &rollout_path(PARENT_THREAD),
                &all[split..],
                Some(&first.next_state),
            )
            .expect("incremental prefix-lookalike batch");
        let complete = CodexAdapter
            .parse_lines(&rollout_path(PARENT_THREAD), &all, None)
            .expect("complete prefix-lookalike batch");

        let private = pseudonymous_session_id(Client::OpenaiCodex, LOOKALIKE);
        assert_ne!(private, LOOKALIKE);
        assert_eq!(
            first
                .next_state
                .get("logical_session_id")
                .and_then(Value::as_str),
            Some(private.as_str())
        );
        assert!(!first.next_state.to_string().contains(LOOKALIKE));
        let incremental = first
            .observations
            .iter()
            .chain(&second.observations)
            .map(|observation| (&observation.event_key, &observation.session_id))
            .collect::<Vec<_>>();
        let complete = complete
            .observations
            .iter()
            .map(|observation| (&observation.event_key, &observation.session_id))
            .collect::<Vec<_>>();
        assert_eq!(incremental, complete);
        assert!(
            incremental
                .iter()
                .all(|(_, session_id)| session_id.as_str() == private)
        );
    }

    #[test]
    fn starts_new_epoch_after_context_reset_and_skips_malformed_tail() {
        let text = format!(
            concat!(
                "{{\"timestamp\":\"2026-07-10T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{parent}\",\"session_id\":\"{root}\",\"cli_version\":\"0.144.0\",\"model_provider\":\"openai\"}}}}\n",
                "{{\"timestamp\":\"2026-07-10T12:00:01Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-reset\"}}}}\n",
                "{{\"timestamp\":\"2026-07-10T12:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":100,\"cached_input_tokens\":20,\"output_tokens\":10,\"reasoning_output_tokens\":4,\"total_tokens\":110}},\"last_token_usage\":{{}},\"model_context_window\":258400}}}}}}\n",
                "{{\"type\":\n",
                "{{\"timestamp\":\"2026-07-10T12:00:03Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":0,\"cached_input_tokens\":0,\"output_tokens\":0,\"reasoning_output_tokens\":0,\"total_tokens\":258400}},\"last_token_usage\":{{}},\"model_context_window\":258400}}}}}}\n",
                "{{\"timestamp\":\"2026-07-10T12:00:04Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":50,\"cached_input_tokens\":10,\"output_tokens\":5,\"reasoning_output_tokens\":2,\"total_tokens\":258455}},\"last_token_usage\":{{}},\"model_context_window\":258400}}}}}}\n"
            ),
            parent = PARENT_THREAD,
            root = ROOT_SESSION
        );
        let batch = CodexAdapter
            .parse_lines(&rollout_path(PARENT_THREAD), &records(&text, 1), None)
            .expect("parse reset fixture");

        assert_eq!(batch.observations.len(), 2);
        assert_eq!(batch.observations[1].usage.input_tokens_total, 50);
        assert_eq!(batch.observations[1].usage.input_tokens_cached, 10);
        assert_eq!(batch.observations[1].usage.input_tokens_uncached, 40);
        assert_eq!(batch.observations[1].usage.output_tokens_total, 5);
        assert!(
            batch
                .warnings
                .iter()
                .any(|warning| warning.code == "codex_counter_reset")
        );
        assert!(
            batch
                .warnings
                .iter()
                .any(|warning| warning.code == "codex_malformed_record")
        );
        assert!(batch.warnings.iter().any(|warning| {
            warning.message.contains("incomplete-tail") && !warning.message.contains('{')
        }));
    }

    #[test]
    fn accounts_optional_cache_write_subsets_without_double_counting() {
        let text = format!(
            concat!(
                "{{\"timestamp\":\"2026-07-10T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{parent}\",\"session_id\":\"{root}\",\"cli_version\":\"0.144.0\",\"model_provider\":\"openai\"}}}}\n",
                "{{\"timestamp\":\"2026-07-10T12:00:01Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-cache\"}}}}\n",
                "{{\"timestamp\":\"2026-07-10T12:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":100,\"cached_input_tokens\":20,\"cache_write_tokens\":10,\"output_tokens\":10,\"reasoning_output_tokens\":2,\"total_tokens\":110}},\"last_token_usage\":{{}},\"model_context_window\":200000}}}}}}\n",
                "{{\"timestamp\":\"2026-07-10T12:00:03Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":160,\"cached_input_tokens\":35,\"cache_write_tokens\":20,\"output_tokens\":20,\"reasoning_output_tokens\":5,\"total_tokens\":180}},\"last_token_usage\":{{}},\"model_context_window\":200000}}}}}}\n"
            ),
            parent = PARENT_THREAD,
            root = ROOT_SESSION
        );
        let batch = CodexAdapter
            .parse_lines(&rollout_path(PARENT_THREAD), &records(&text, 1), None)
            .expect("parse cache fixture");

        assert_eq!(batch.observations.len(), 2);
        let first = &batch.observations[0];
        assert_eq!(first.usage.input_tokens_uncached, 70);
        assert_eq!(first.usage.input_tokens_cached, 20);
        assert_eq!(first.usage.cache_write_unknown_tokens, 10);
        assert_eq!(first.coverage, CoverageStatus::CompleteKnown);
        assert_eq!(first.dimensions.cache_write_data_complete, Some(true));

        let second = &batch.observations[1];
        assert_eq!(second.usage.input_tokens_total, 60);
        assert_eq!(second.usage.input_tokens_cached, 15);
        assert_eq!(second.usage.cache_write_unknown_tokens, 10);
        assert_eq!(second.usage.input_tokens_uncached, 35);
    }

    #[test]
    fn copied_fork_boundaries_have_the_same_dedupe_key() {
        let usage = serde_json::json!({
            "input_tokens": 100,
            "cached_input_tokens": 20,
            "output_tokens": 10,
            "reasoning_output_tokens": 4,
            "total_tokens": 110
        });
        let parent = jsonl(&[
            serde_json::json!({
                "timestamp": "2026-07-09T10:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": PARENT_THREAD,
                    "session_id": ROOT_SESSION,
                    "cli_version": "0.144.0",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-07-09T10:00:01Z",
                "type": "turn_context",
                "payload": { "model": "gpt-fork" }
            }),
            serde_json::json!({
                "timestamp": "2026-07-09T10:00:02Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": usage.clone(),
                        "last_token_usage": usage.clone()
                    }
                }
            }),
        ]);
        let child = jsonl(&[
            serde_json::json!({
                "timestamp": "2026-07-10T10:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": CHILD_THREAD,
                    "session_id": ROOT_SESSION,
                    "forked_from_id": PARENT_THREAD,
                    "parent_thread_id": PARENT_THREAD,
                    "cli_version": "0.144.0",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-07-10T10:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": PARENT_THREAD,
                    "session_id": ROOT_SESSION,
                    "cli_version": "0.144.0",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-07-10T10:00:00Z",
                "type": "turn_context",
                "payload": { "model": "gpt-fork" }
            }),
            serde_json::json!({
                "timestamp": "2026-07-10T10:00:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": usage.clone(),
                        "last_token_usage": usage
                    }
                }
            }),
        ]);
        let parent_batch = CodexAdapter
            .parse_lines(&rollout_path(PARENT_THREAD), &records(&parent, 1), None)
            .expect("parent");
        let child_batch = CodexAdapter
            .parse_lines(&rollout_path(CHILD_THREAD), &records(&child, 1), None)
            .expect("child");

        assert_eq!(parent_batch.observations.len(), 1);
        assert_eq!(child_batch.observations.len(), 1);
        assert_eq!(
            parent_batch.observations[0].event_key,
            child_batch.observations[0].event_key
        );
        assert_eq!(
            child_batch.observations[0].session_id,
            pseudonymous_session_id(Client::OpenaiCodex, ROOT_SESSION)
        );
        assert_eq!(
            parent_batch.observations[0].occurred_at,
            "2026-07-09T10:00:02Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert!(child_batch.observations[0].occurred_at > parent_batch.observations[0].occurred_at);
    }

    #[test]
    fn warns_on_unknown_usage_fields_but_keeps_known_counters() {
        let text = jsonl(&[
            serde_json::json!({
                "timestamp": "2026-07-10T11:59:59Z",
                "type": "secret_record_type_canary"
            }),
            serde_json::json!({
                "timestamp": "2026-07-10T12:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": PARENT_THREAD,
                    "session_id": ROOT_SESSION,
                    "cli_version": "99.0.0",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-07-10T12:00:01Z",
                "type": "turn_context",
                "payload": { "model": "gpt-future" }
            }),
            serde_json::json!({
                "timestamp": "2026-07-10T12:00:02Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 5,
                            "cached_input_tokens": 1,
                            "output_tokens": 2,
                            "reasoning_output_tokens": 1,
                            "total_tokens": 7,
                            "future_tokens": 9
                        },
                        "last_token_usage": {}
                    }
                }
            }),
        ]);
        let batch = CodexAdapter
            .parse_lines(&rollout_path(PARENT_THREAD), &records(&text, 1), None)
            .expect("future format");
        assert_eq!(batch.observations.len(), 1);
        assert!(
            batch
                .warnings
                .iter()
                .any(|warning| warning.code == "codex_format_drift")
        );
        assert!(!format!("{:?}", batch.warnings).contains("future_tokens"));
        assert!(!format!("{:?}", batch.warnings).contains("secret_record_type_canary"));
    }

    #[test]
    fn discovers_active_archived_and_compressed_rollouts() {
        let temp = TempDir::new().expect("tempdir");
        let active = temp.path().join("sessions/2026/07/10");
        let archived = temp.path().join("archived_sessions");
        fs::create_dir_all(&active).unwrap();
        fs::create_dir_all(&archived).unwrap();

        let plain = active.join(format!("rollout-2026-07-10T12-00-00-{PARENT_THREAD}.jsonl"));
        let duplicate_compressed = active.join(format!(
            "rollout-2026-07-10T12-00-00-{PARENT_THREAD}.jsonl.zst"
        ));
        let archived_compressed = archived.join(format!(
            "rollout-2026-07-11T12-00-00-{CHILD_THREAD}.jsonl.zst"
        ));
        fs::write(&plain, "").unwrap();
        fs::write(&duplicate_compressed, "").unwrap();
        fs::write(&archived_compressed, "").unwrap();
        fs::write(active.join("notes.jsonl"), "").unwrap();

        let config = Config {
            codex_home: Some(temp.path().to_path_buf()),
            ..Config::default()
        };
        let sources = CodexAdapter.discover(&config).expect("discover");

        assert_eq!(sources.len(), 2);
        let plain = plain.canonicalize().unwrap();
        let archived_compressed = archived_compressed.canonicalize().unwrap();
        let duplicate_compressed = duplicate_compressed.canonicalize().unwrap();
        assert!(
            sources.iter().any(|source| {
                source.path.canonicalize().unwrap() == plain && !source.compressed
            })
        );
        assert!(sources.iter().any(|source| {
            source.path.canonicalize().unwrap() == archived_compressed && source.compressed
        }));
        assert!(
            !sources
                .iter()
                .any(|source| source.path == duplicate_compressed)
        );
    }

    #[test]
    fn non_directory_session_root_is_a_discovery_error() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join(ACTIVE_SESSIONS_DIR), "not a directory").unwrap();
        let config = Config {
            codex_home: Some(temp.path().to_path_buf()),
            ..Config::default()
        };

        assert!(CodexAdapter.discover(&config).is_err());
    }
}
