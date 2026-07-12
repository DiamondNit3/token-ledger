use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use directories::BaseDirs;
use serde::Deserialize;
use serde_json::Value;

use crate::adapters::{DiscoveryRequest, DiscoveryResult, SourceAdapter, discover_bounded_files};
use crate::config::{Config, PathOrigin, ResolvedPath};
use crate::model::{
    Client, CoverageStatus, DimensionValueProvenance, LineRecord, ParseBatch, PricingDimensions,
    ScanWarning, SourceSpec, UsageObservation, UsageQuality, UsageVector, stable_id,
};

const PARSER_VERSION: &str = "claude-jsonl-v1";
const PROVIDER: &str = "anthropic";
const CACHE_TTL_INCOMPLETE_WARNING: &str = "cache-write TTL classification is incomplete";

#[derive(Debug, Default)]
pub struct ClaudeAdapter;

impl ClaudeAdapter {
    /// Resolve the Claude data root using CLI > environment > config > default.
    pub fn resolve_root(config: &Config) -> Result<PathBuf> {
        Ok(Self::resolve_root_with_origin(config)?.path)
    }

    pub fn resolve_root_with_origin(config: &Config) -> Result<ResolvedPath> {
        let home = BaseDirs::new()
            .context("could not determine the home directory for Claude Code discovery")?;
        resolve_root_candidates(
            config.claude_root_override.as_deref(),
            env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            config.claude_root.as_deref(),
            home.home_dir().join(".claude"),
        )
    }
}

fn resolve_root_candidates(
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
    Ok(ResolvedPath {
        path: expand_tilde(&path)?,
        origin,
    })
}

impl SourceAdapter for ClaudeAdapter {
    fn client(&self) -> Client {
        Client::ClaudeCode
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn discover_bounded(
        &self,
        config: &Config,
        request: DiscoveryRequest,
    ) -> Result<DiscoveryResult> {
        let resolved_root = Self::resolve_root_with_origin(config)?;
        let root_exists = resolved_root.path.try_exists().with_context(|| {
            format!(
                "failed to inspect Claude Code root {}",
                resolved_root.path.display()
            )
        })?;
        if !root_exists {
            if resolved_root.origin == PathOrigin::Default {
                // A machine that has never used Claude Code normally has no
                // platform-default root; that is not a discovery failure.
                return Ok(DiscoveryResult::default());
            }
            bail!("selected Claude Code root does not exist");
        }
        if !fs::metadata(&resolved_root.path)
            .with_context(|| {
                format!(
                    "failed to inspect Claude Code root metadata {}",
                    resolved_root.path.display()
                )
            })?
            .is_dir()
        {
            bail!("selected Claude Code root is not a directory");
        }

        let projects_root = resolved_root.path.join("projects");
        if !projects_root.try_exists().with_context(|| {
            format!(
                "failed to inspect Claude Code projects directory {}",
                projects_root.display()
            )
        })? {
            return Ok(DiscoveryResult::default());
        }

        // Validate the top-level directory separately so a missing default is
        // distinct from a configured root that cannot be enumerated.
        fs::read_dir(&projects_root).with_context(|| {
            format!(
                "failed to read Claude Code projects directory {}",
                projects_root.display()
            )
        })?;

        discover_bounded_files(std::slice::from_ref(&projects_root), request, |path| {
            if !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            {
                return Ok(None);
            }
            Ok(Some(SourceSpec {
                path: path.to_path_buf(),
                trusted_root: projects_root.clone(),
                client: Client::ClaudeCode,
                compressed: false,
            }))
        })
    }

    fn parse_lines(
        &self,
        path: &Path,
        lines: &[LineRecord],
        _previous_state: Option<&Value>,
    ) -> Result<ParseBatch> {
        let mut batch = ParseBatch::default();
        let mut observations = BTreeMap::<String, PendingObservation>::new();
        let session_hint = session_id_from_path(path);

        for line in lines {
            if line.text.trim().is_empty() {
                continue;
            }

            let envelope = match serde_json::from_str::<ClaudeEnvelope>(&line.text) {
                Ok(envelope) => envelope,
                Err(error) => {
                    let (code, message) = if error.is_eof() {
                        (
                            "claude_incomplete_record",
                            "skipped an incomplete Claude Code JSONL record",
                        )
                    } else {
                        (
                            "claude_malformed_record",
                            "skipped a malformed Claude Code JSONL record",
                        )
                    };
                    // Do not attach serde's diagnostic: future schema errors can
                    // quote a value, while source records may contain secrets.
                    batch
                        .warnings
                        .push(ScanWarning::new(code, message).at(line.locator()));
                    continue;
                }
            };

            if envelope.record_type.as_deref() != Some("assistant") {
                continue;
            }

            let Some(candidate) = candidate_from_envelope(
                path,
                line,
                envelope,
                session_hint.as_deref(),
                &mut batch.warnings,
            ) else {
                continue;
            };

            match observations.get_mut(&candidate.group_key) {
                Some(existing) => existing.merge(candidate),
                None => {
                    observations.insert(candidate.group_key.clone(), candidate);
                }
            }
        }

        for (_, mut pending) in observations {
            if pending.record_count > 1 {
                pending.observation.source_locator = format!(
                    "{}; merged {} records",
                    pending.observation.source_locator, pending.record_count
                );
                pending
                    .observation
                    .warnings
                    .push("deduplicated repeated Claude response records".to_string());
            }
            batch.observations.push(pending.observation);
        }

        batch.observations.sort_by(|left, right| {
            left.occurred_at
                .cmp(&right.occurred_at)
                .then_with(|| left.event_key.cmp(&right.event_key))
        });
        // Claude scan warnings are emitted only when a candidate usage record
        // was malformed or lacked fields required to account for it.
        batch.incomplete = !batch.warnings.is_empty();
        Ok(batch)
    }
}

fn expand_tilde(path: &Path) -> Result<PathBuf> {
    let Ok(remainder) = path.strip_prefix("~") else {
        return Ok(path.to_path_buf());
    };
    let home = BaseDirs::new().context("could not expand '~' without a home directory")?;
    Ok(home.home_dir().join(remainder))
}

fn session_id_from_path(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("subagents"))
    {
        return parent
            .parent()?
            .file_name()?
            .to_str()
            .map(ToOwned::to_owned);
    }
    path.file_stem()?.to_str().map(ToOwned::to_owned)
}

fn candidate_from_envelope(
    path: &Path,
    line: &LineRecord,
    envelope: ClaudeEnvelope,
    session_hint: Option<&str>,
    scan_warnings: &mut Vec<ScanWarning>,
) -> Option<PendingObservation> {
    let locator = line.locator();
    let Some(timestamp) = envelope.timestamp.as_deref() else {
        scan_warnings.push(
            ScanWarning::new(
                "claude_missing_timestamp",
                "skipped a Claude Code usage record without a completion timestamp",
            )
            .at(locator),
        );
        return None;
    };
    let Ok(occurred_at) = DateTime::parse_from_rfc3339(timestamp) else {
        scan_warnings.push(
            ScanWarning::new(
                "claude_invalid_timestamp",
                "skipped a Claude Code usage record with an invalid completion timestamp",
            )
            .at(locator),
        );
        return None;
    };
    let occurred_at = occurred_at.with_timezone(&Utc);

    let Some(message) = envelope.message else {
        scan_warnings.push(
            ScanWarning::new(
                "claude_missing_usage",
                "skipped a Claude Code assistant record without a usage envelope",
            )
            .at(locator),
        );
        return None;
    };
    let Some(usage) = message.usage else {
        scan_warnings.push(
            ScanWarning::new(
                "claude_missing_usage",
                "skipped a Claude Code assistant record without a usage envelope",
            )
            .at(locator),
        );
        return None;
    };
    if !usage.has_supported_fields() {
        scan_warnings.push(
            ScanWarning::new(
                "claude_empty_usage",
                "skipped a Claude Code assistant record without supported usage counters",
            )
            .at(locator),
        );
        return None;
    }

    let mut warnings = Vec::new();
    let mut coverage = CoverageStatus::CompleteKnown;

    let provider_message_id = nonempty(message.id);
    if provider_message_id.is_none() {
        coverage = CoverageStatus::PartialKnown;
        warnings.push(
            "provider message ID was unavailable; deduplication uses a fallback identity"
                .to_string(),
        );
    }
    let request_id = nonempty(envelope.request_id);

    let session_id = nonempty(envelope.session_id)
        .or_else(|| session_hint.map(ToOwned::to_owned))
        .unwrap_or_else(|| {
            coverage = CoverageStatus::PartialKnown;
            warnings
                .push("session ID was unavailable; a private stable fallback was used".to_string());
            stable_id(&["claude-session", &path.to_string_lossy()])
        });

    let raw_model = nonempty(message.model).unwrap_or_else(|| {
        coverage = CoverageStatus::PartialKnown;
        warnings.push("model was unavailable; this event cannot be priced".to_string());
        "unknown".to_string()
    });

    let cache_5m = usage
        .cache_creation
        .as_ref()
        .and_then(|cache| cache.ephemeral_5m_input_tokens)
        .unwrap_or(0);
    let cache_1h = usage
        .cache_creation
        .as_ref()
        .and_then(|cache| cache.ephemeral_1h_input_tokens)
        .unwrap_or(0);
    let classified_cache_write = cache_5m.saturating_add(cache_1h);
    let reported_cache_write = usage
        .cache_creation_input_tokens
        .unwrap_or(classified_cache_write);
    let cache_write_unknown = reported_cache_write.saturating_sub(classified_cache_write);

    let cache_write_data_complete = match (
        usage.cache_creation_input_tokens,
        usage.cache_creation.as_ref(),
    ) {
        (Some(total), Some(_)) if total == classified_cache_write => true,
        (Some(0), None) => true,
        (None, Some(_)) => true,
        _ => false,
    };

    if classified_cache_write > reported_cache_write {
        coverage = CoverageStatus::PartialKnown;
        warnings.push(
            "cache-write TTL counters exceeded the reported cache-write total; known TTL counters were retained"
                .to_string(),
        );
    } else if !cache_write_data_complete {
        coverage = CoverageStatus::PartialKnown;
        warnings.push(CACHE_TTL_INCOMPLETE_WARNING.to_string());
    }

    if usage.input_tokens.is_none()
        || usage.output_tokens.is_none()
        || usage.cache_read_input_tokens.is_none()
    {
        coverage = CoverageStatus::PartialKnown;
        warnings.push("one or more standard token counters were unavailable".to_string());
    }

    let input_uncached = usage.input_tokens.unwrap_or(0);
    let input_cached = usage.cache_read_input_tokens.unwrap_or(0);
    let usage_vector = UsageVector {
        input_tokens_total: input_uncached
            .saturating_add(input_cached)
            .saturating_add(cache_5m)
            .saturating_add(cache_1h)
            .saturating_add(cache_write_unknown),
        input_tokens_uncached: input_uncached,
        input_tokens_cached: input_cached,
        cache_write_5m_tokens: cache_5m,
        cache_write_1h_tokens: cache_1h,
        cache_write_unknown_tokens: cache_write_unknown,
        output_tokens_total: usage.output_tokens.unwrap_or(0),
        reasoning_output_tokens: 0,
        web_search_requests: usage
            .server_tool_use
            .as_ref()
            .and_then(|tools| tools.web_search_requests)
            .unwrap_or(0),
        web_fetch_requests: usage
            .server_tool_use
            .as_ref()
            .and_then(|tools| tools.web_fetch_requests)
            .unwrap_or(0),
    };

    let group_key = provider_message_id
        .as_ref()
        .map(|id| format!("message:{id}"))
        .or_else(|| request_id.as_ref().map(|id| format!("request:{id}")))
        .or_else(|| nonempty(envelope.record_id).map(|id| format!("record:{id}")))
        .unwrap_or_else(|| {
            stable_id(&[
                "claude-record",
                &path.to_string_lossy(),
                &line.line_number.to_string(),
            ])
        });
    // The provider message ID is the canonical dedupe boundary. Request ID is
    // retained separately as a pricing/route dimension so records where it is
    // temporarily absent cannot split one provider response into two events.
    let event_key = group_key.clone();

    Some(PendingObservation {
        group_key,
        record_count: 1,
        reported_cache_write_tokens: reported_cache_write,
        cache_write_total_seen: usage.cache_creation_input_tokens.is_some(),
        cache_write_breakdown_seen: usage.cache_creation.is_some(),
        observation: UsageObservation {
            event_key,
            client: Client::ClaudeCode,
            session_id,
            usage_event_index: None,
            provider_message_id,
            occurred_at,
            raw_model,
            provider: PROVIDER.to_string(),
            usage: usage_vector,
            dimensions: PricingDimensions {
                provider_request_id: request_id,
                service_tier_provenance: usage
                    .service_tier
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                    .map(|_| DimensionValueProvenance::SourceObserved),
                service_tier: nonempty(usage.service_tier),
                speed_provenance: usage
                    .speed
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                    .map(|_| DimensionValueProvenance::SourceObserved),
                speed: nonempty(usage.speed),
                inference_geo_provenance: usage
                    .inference_geo
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                    .map(|_| DimensionValueProvenance::SourceObserved),
                inference_geo: nonempty(usage.inference_geo),
                cache_write_data_complete: Some(cache_write_data_complete),
                ..PricingDimensions::default()
            },
            quality: UsageQuality::Exact,
            coverage,
            source_locator: line.locator(),
            parser_version: PARSER_VERSION.to_string(),
            warnings,
        },
    })
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn recompute_claude_input_total(usage: &mut UsageVector) {
    usage.input_tokens_total = usage
        .input_tokens_uncached
        .saturating_add(usage.input_tokens_cached)
        .saturating_add(usage.cache_write_5m_tokens)
        .saturating_add(usage.cache_write_1h_tokens)
        .saturating_add(usage.cache_write_unknown_tokens);
}

#[derive(Debug)]
struct PendingObservation {
    group_key: String,
    record_count: usize,
    reported_cache_write_tokens: u64,
    cache_write_total_seen: bool,
    cache_write_breakdown_seen: bool,
    observation: UsageObservation,
}

impl PendingObservation {
    fn merge(&mut self, candidate: Self) {
        self.record_count += candidate.record_count;

        if candidate.observation.occurred_at < self.observation.occurred_at {
            self.observation.occurred_at = candidate.observation.occurred_at;
            self.observation.source_locator = candidate.observation.source_locator.clone();
        }

        self.reported_cache_write_tokens = self
            .reported_cache_write_tokens
            .max(candidate.reported_cache_write_tokens);
        self.cache_write_total_seen |= candidate.cache_write_total_seen;
        self.cache_write_breakdown_seen |= candidate.cache_write_breakdown_seen;

        self.observation
            .usage
            .componentwise_max(&candidate.observation.usage);
        // An older copy may expose only the aggregate cache-write count while a
        // later copy classifies the same tokens by TTL. Do not retain both the
        // earlier "unknown" amount and the later classified amount.
        let classified_cache_write = self
            .observation
            .usage
            .cache_write_5m_tokens
            .saturating_add(self.observation.usage.cache_write_1h_tokens);
        self.observation.usage.cache_write_unknown_tokens = self
            .reported_cache_write_tokens
            .saturating_sub(classified_cache_write);
        let cache_write_complete = (self.cache_write_breakdown_seen
            && classified_cache_write == self.reported_cache_write_tokens)
            || (self.cache_write_total_seen && self.reported_cache_write_tokens == 0);
        self.observation.dimensions.cache_write_data_complete = Some(cache_write_complete);
        // Claude input classes are disjoint. Componentwise maxima can combine
        // fields from evolving copies, so recompute rather than max the derived sum.
        recompute_claude_input_total(&mut self.observation.usage);

        merge_option(
            &mut self.observation.provider_message_id,
            candidate.observation.provider_message_id,
            &mut self.observation.warnings,
            "provider message ID",
        );
        merge_string(
            &mut self.observation.session_id,
            candidate.observation.session_id,
            &mut self.observation.warnings,
            "session ID",
            false,
        );
        merge_string(
            &mut self.observation.raw_model,
            candidate.observation.raw_model,
            &mut self.observation.warnings,
            "model",
            true,
        );
        merge_option(
            &mut self.observation.dimensions.provider_request_id,
            candidate.observation.dimensions.provider_request_id,
            &mut self.observation.warnings,
            "provider request ID",
        );
        merge_option(
            &mut self.observation.dimensions.service_tier,
            candidate.observation.dimensions.service_tier,
            &mut self.observation.warnings,
            "service tier",
        );
        merge_option(
            &mut self.observation.dimensions.speed,
            candidate.observation.dimensions.speed,
            &mut self.observation.warnings,
            "speed",
        );
        merge_option(
            &mut self.observation.dimensions.inference_geo,
            candidate.observation.dimensions.inference_geo,
            &mut self.observation.warnings,
            "inference geography",
        );

        if candidate.observation.coverage != CoverageStatus::CompleteKnown {
            self.observation.coverage = CoverageStatus::PartialKnown;
        }
        self.observation
            .warnings
            .extend(candidate.observation.warnings);
        if cache_write_complete {
            self.observation
                .warnings
                .retain(|warning| warning != CACHE_TTL_INCOMPLETE_WARNING);
        }
        self.observation.warnings.sort();
        self.observation.warnings.dedup();
        self.observation.coverage = if self.observation.warnings.is_empty() {
            CoverageStatus::CompleteKnown
        } else {
            CoverageStatus::PartialKnown
        };
    }
}

fn merge_option(
    current: &mut Option<String>,
    candidate: Option<String>,
    warnings: &mut Vec<String>,
    label: &str,
) {
    match (&*current, candidate) {
        (None, Some(value)) => *current = Some(value),
        (Some(existing), Some(value)) if existing != &value => warnings.push(format!(
            "duplicate records disagreed on {label}; the first value was retained"
        )),
        _ => {}
    }
}

fn merge_string(
    current: &mut String,
    candidate: String,
    warnings: &mut Vec<String>,
    label: &str,
    replace_unknown: bool,
) {
    if replace_unknown && current == "unknown" && candidate != "unknown" {
        *current = candidate;
    } else if current != &candidate {
        warnings.push(format!(
            "duplicate records disagreed on {label}; the first value was retained"
        ));
    }
}

// These structures intentionally allowlist only accounting-envelope fields.
// serde skips prompt, response, tool input/result, cwd, and other transcript data
// without retaining them in the normalized observation.
#[derive(Debug, Deserialize)]
struct ClaudeEnvelope {
    #[serde(rename = "type")]
    record_type: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    #[serde(rename = "uuid")]
    record_id: Option<String>,
    message: Option<ClaudeMessage>,
}

#[derive(Debug, Deserialize)]
struct ClaudeMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<ClaudeUsage>,
}

#[derive(Debug, Deserialize)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation: Option<ClaudeCacheCreation>,
    server_tool_use: Option<ClaudeServerToolUse>,
    service_tier: Option<String>,
    speed: Option<String>,
    inference_geo: Option<String>,
}

impl ClaudeUsage {
    fn has_supported_fields(&self) -> bool {
        self.input_tokens.is_some()
            || self.cache_creation_input_tokens.is_some()
            || self.cache_read_input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_creation.is_some()
            || self.server_tool_use.is_some()
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeCacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ClaudeServerToolUse {
    web_search_requests: Option<u64>,
    web_fetch_requests: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn record(line_number: u64, text: impl Into<String>) -> LineRecord {
        LineRecord {
            line_number,
            byte_start: line_number * 100,
            byte_end: line_number * 100 + 99,
            text: text.into(),
        }
    }

    #[test]
    fn root_candidates_follow_cli_environment_config_default_precedence() {
        let default = PathBuf::from("C:/default-claude");
        let configured = PathBuf::from("C:/configured-claude");
        let environment = PathBuf::from("C:/environment-claude");
        let cli = PathBuf::from("C:/cli-claude");

        let resolved = resolve_root_candidates(
            Some(&cli),
            Some(environment.clone()),
            Some(&configured),
            default.clone(),
        )
        .unwrap();
        assert_eq!(resolved.path, cli);
        assert_eq!(resolved.origin, PathOrigin::Cli);

        let resolved = resolve_root_candidates(
            None,
            Some(environment.clone()),
            Some(&configured),
            default.clone(),
        )
        .unwrap();
        assert_eq!(resolved.path, environment);
        assert_eq!(resolved.origin, PathOrigin::Environment);

        let resolved =
            resolve_root_candidates(None, None, Some(&configured), default.clone()).unwrap();
        assert_eq!(resolved.path, configured);
        assert_eq!(resolved.origin, PathOrigin::Config);

        let resolved = resolve_root_candidates(None, None, None, default.clone()).unwrap();
        assert_eq!(resolved.path, default);
        assert_eq!(resolved.origin, PathOrigin::Default);
    }

    #[test]
    fn discovers_main_and_subagent_jsonl_recursively() {
        let temp = tempdir().unwrap();
        let root = temp.path().join(".claude");
        let project = root.join("projects").join("project-a");
        let subagents = project.join("session-a").join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(project.join("session-a.jsonl"), "").unwrap();
        fs::write(subagents.join("agent-one.jsonl"), "").unwrap();
        fs::write(project.join("ignore.txt"), "").unwrap();

        let config = Config {
            claude_root: Some(root),
            ..Config::default()
        };
        let sources = ClaudeAdapter.discover(&config).unwrap();

        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .iter()
                .all(|source| source.client == Client::ClaudeCode)
        );
        assert!(
            sources
                .iter()
                .any(|source| source.path.ends_with("session-a.jsonl"))
        );
        assert!(
            sources
                .iter()
                .any(|source| source.path.ends_with("agent-one.jsonl"))
        );
    }

    #[test]
    fn non_directory_projects_root_is_a_discovery_error() {
        let temp = tempdir().unwrap();
        let root = temp.path().join(".claude");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("projects"), "not a directory").unwrap();
        let config = Config {
            claude_root: Some(root),
            ..Config::default()
        };

        assert!(ClaudeAdapter.discover(&config).is_err());
    }

    #[test]
    fn explicit_missing_root_is_a_discovery_error() {
        let temp = tempdir().unwrap();
        let config = Config {
            claude_root_override: Some(temp.path().join("missing-claude-root")),
            ..Config::default()
        };

        assert!(ClaudeAdapter.discover(&config).is_err());
    }

    #[test]
    fn normalizes_and_merges_evolving_message_records_without_content() {
        let path = Path::new("projects/project-a/session-a.jsonl");
        let sensitive = "UNIQUE_PROMPT_AND_TOOL_RESULT_MUST_NOT_SURVIVE";
        let first = format!(
            r#"{{"type":"assistant","timestamp":"2026-07-10T12:00:00Z","sessionId":"session-a","requestId":"req-a","uuid":"row-a","toolUseResult":"{sensitive}","message":{{"id":"msg-a","model":"claude-sonnet-4-6","content":[{{"type":"text","text":"{sensitive}"}}],"usage":{{"input_tokens":10,"cache_creation_input_tokens":5,"cache_read_input_tokens":3,"output_tokens":2,"cache_creation":{{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":0}},"service_tier":"standard","inference_geo":"us"}}}}}}"#
        );
        let second = format!(
            r#"{{"type":"assistant","timestamp":"2026-07-10T11:59:00Z","sessionId":"session-a","requestId":"req-a","message":{{"id":"msg-a","model":"claude-sonnet-4-6","content":"{sensitive}","usage":{{"input_tokens":10,"cache_creation_input_tokens":7,"cache_read_input_tokens":3,"output_tokens":9,"cache_creation":{{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":2}},"server_tool_use":{{"web_search_requests":1,"web_fetch_requests":2}},"service_tier":"standard","speed":"fast","inference_geo":"us"}}}}}}"#
        );

        let batch = ClaudeAdapter
            .parse_lines(path, &[record(1, first), record(2, second)], None)
            .unwrap();

        assert_eq!(batch.observations.len(), 1);
        let observation = &batch.observations[0];
        assert_eq!(observation.event_key, "message:msg-a");
        assert_eq!(observation.provider_message_id.as_deref(), Some("msg-a"));
        assert_eq!(
            observation.dimensions.provider_request_id.as_deref(),
            Some("req-a")
        );
        assert_eq!(observation.session_id, "session-a");
        assert_eq!(observation.raw_model, "claude-sonnet-4-6");
        assert_eq!(
            observation.occurred_at.to_rfc3339(),
            "2026-07-10T11:59:00+00:00"
        );
        assert_eq!(observation.usage.input_tokens_uncached, 10);
        assert_eq!(observation.usage.input_tokens_cached, 3);
        assert_eq!(observation.usage.cache_write_5m_tokens, 5);
        assert_eq!(observation.usage.cache_write_1h_tokens, 2);
        assert_eq!(observation.usage.input_tokens_total, 20);
        assert_eq!(observation.usage.output_tokens_total, 9);
        assert_eq!(observation.usage.web_search_requests, 1);
        assert_eq!(observation.usage.web_fetch_requests, 2);
        assert_eq!(observation.dimensions.speed.as_deref(), Some("fast"));
        assert_eq!(observation.dimensions.inference_geo.as_deref(), Some("us"));
        assert!(observation.source_locator.contains("merged 2 records"));
        assert!(
            observation
                .warnings
                .iter()
                .any(|warning| warning == "deduplicated repeated Claude response records")
        );

        let normalized = serde_json::to_string(&batch.observations).unwrap();
        assert!(!normalized.contains(sensitive));
        assert!(!format!("{batch:?}").contains(sensitive));
    }

    #[test]
    fn retains_unclassified_cache_writes_as_unknown() {
        let line = record(
            1,
            r#"{"type":"assistant","timestamp":"2026-07-10T12:00:00Z","sessionId":"session-a","requestId":"req-a","message":{"id":"msg-a","model":"claude-opus-4-6","usage":{"input_tokens":1,"cache_creation_input_tokens":4,"cache_read_input_tokens":2,"output_tokens":3}}}"#,
        );

        let batch = ClaudeAdapter
            .parse_lines(Path::new("session-a.jsonl"), &[line], None)
            .unwrap();
        let observation = &batch.observations[0];
        assert_eq!(observation.usage.cache_write_unknown_tokens, 4);
        assert_eq!(observation.usage.input_tokens_total, 7);
        assert_eq!(
            observation.dimensions.cache_write_data_complete,
            Some(false)
        );
        assert_eq!(observation.coverage, CoverageStatus::PartialKnown);
    }

    #[test]
    fn later_ttl_breakdown_reclassifies_the_same_cache_write_tokens() {
        let aggregate_only = record(
            1,
            r#"{"type":"assistant","timestamp":"2026-07-10T12:00:00Z","sessionId":"session-a","requestId":"req-a","message":{"id":"msg-a","model":"claude-opus-4-6","usage":{"input_tokens":1,"cache_creation_input_tokens":7,"cache_read_input_tokens":0,"output_tokens":1}}}"#,
        );
        let classified = record(
            2,
            r#"{"type":"assistant","timestamp":"2026-07-10T12:00:01Z","sessionId":"session-a","requestId":"req-a","message":{"id":"msg-a","model":"claude-opus-4-6","usage":{"input_tokens":1,"cache_creation_input_tokens":7,"cache_read_input_tokens":0,"output_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":2}}}}"#,
        );

        let batch = ClaudeAdapter
            .parse_lines(
                Path::new("session-a.jsonl"),
                &[aggregate_only, classified],
                None,
            )
            .unwrap();
        let usage = &batch.observations[0].usage;
        assert_eq!(usage.cache_write_5m_tokens, 5);
        assert_eq!(usage.cache_write_1h_tokens, 2);
        assert_eq!(usage.cache_write_unknown_tokens, 0);
        assert_eq!(usage.input_tokens_total, 8);
        assert_eq!(
            batch.observations[0].dimensions.cache_write_data_complete,
            Some(true)
        );
        assert_eq!(
            batch.observations[0].coverage,
            CoverageStatus::CompleteKnown
        );
    }

    #[test]
    fn malformed_and_incomplete_records_are_safe_warnings() {
        let secret = "NEVER_ECHO_THIS_TRANSCRIPT_FRAGMENT";
        let lines = vec![
            record(1, "not-json"),
            record(
                2,
                format!(r#"{{"type":"assistant","message":{{"content":"{secret}""#),
            ),
            record(3, r#"{"type":"user","message":{"content":"also private"}}"#),
        ];

        let batch = ClaudeAdapter
            .parse_lines(Path::new("session-a.jsonl"), &lines, None)
            .unwrap();
        assert!(batch.observations.is_empty());
        assert_eq!(batch.warnings.len(), 2);
        assert!(
            batch
                .warnings
                .iter()
                .any(|warning| warning.code == "claude_malformed_record")
        );
        assert!(
            batch
                .warnings
                .iter()
                .any(|warning| warning.code == "claude_incomplete_record")
        );
        assert!(!format!("{batch:?}").contains(secret));
    }

    #[test]
    fn derives_parent_session_for_subagent_transcripts() {
        let line = record(
            1,
            r#"{"type":"assistant","timestamp":"2026-07-10T12:00:00Z","requestId":"req-a","message":{"id":"msg-a","model":"claude-haiku-4-5","usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}"#,
        );
        let path = Path::new("projects/project-a/session-parent/subagents/agent-child.jsonl");
        let batch = ClaudeAdapter.parse_lines(path, &[line], None).unwrap();
        assert_eq!(batch.observations[0].session_id, "session-parent");
    }
}
