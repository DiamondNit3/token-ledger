use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    ClaudeCode,
    OpenaiCodex,
}

impl Client {
    pub const ALL: [Self; 2] = [Self::ClaudeCode, Self::OpenaiCodex];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::OpenaiCodex => "openai_codex",
        }
    }
}

impl fmt::Display for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Client {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude_code" | "claude-code" => Ok(Self::ClaudeCode),
            "codex" | "openai_codex" | "openai-codex" => Ok(Self::OpenaiCodex),
            _ => anyhow::bail!("unsupported client '{value}'; use claude or codex"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageQuality {
    Exact,
    Derived,
    Heuristic,
    Unresolved,
}

impl UsageQuality {
    pub fn rank(self) -> i64 {
        match self {
            Self::Exact => 3,
            Self::Derived => 2,
            Self::Heuristic => 1,
            Self::Unresolved => 0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Derived => "derived",
            Self::Heuristic => "heuristic",
            Self::Unresolved => "unresolved",
        }
    }

    pub fn from_rank(rank: i64) -> Self {
        match rank {
            3.. => Self::Exact,
            2 => Self::Derived,
            1 => Self::Heuristic,
            _ => Self::Unresolved,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    CompleteKnown,
    PartialKnown,
    Unknown,
}

impl CoverageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompleteKnown => "complete_known",
            Self::PartialKnown => "partial_known",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageVector {
    pub input_tokens_total: u64,
    pub input_tokens_uncached: u64,
    pub input_tokens_cached: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub cache_write_unknown_tokens: u64,
    pub output_tokens_total: u64,
    pub reasoning_output_tokens: u64,
    pub web_search_requests: u64,
    pub web_fetch_requests: u64,
}

impl UsageVector {
    pub fn cache_write_tokens(&self) -> u64 {
        self.cache_write_5m_tokens
            .saturating_add(self.cache_write_1h_tokens)
            .saturating_add(self.cache_write_unknown_tokens)
    }

    pub fn total_billable_tokens(&self) -> u64 {
        self.input_tokens_uncached
            .saturating_add(self.input_tokens_cached)
            .saturating_add(self.cache_write_tokens())
            .saturating_add(self.output_tokens_total)
    }

    pub fn componentwise_max(&mut self, other: &Self) {
        self.input_tokens_total = self.input_tokens_total.max(other.input_tokens_total);
        self.input_tokens_uncached = self.input_tokens_uncached.max(other.input_tokens_uncached);
        self.input_tokens_cached = self.input_tokens_cached.max(other.input_tokens_cached);
        self.cache_write_5m_tokens = self.cache_write_5m_tokens.max(other.cache_write_5m_tokens);
        self.cache_write_1h_tokens = self.cache_write_1h_tokens.max(other.cache_write_1h_tokens);
        self.cache_write_unknown_tokens = self
            .cache_write_unknown_tokens
            .max(other.cache_write_unknown_tokens);
        self.output_tokens_total = self.output_tokens_total.max(other.output_tokens_total);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .max(other.reasoning_output_tokens);
        self.web_search_requests = self.web_search_requests.max(other.web_search_requests);
        self.web_fetch_requests = self.web_fetch_requests.max(other.web_fetch_requests);
    }

    pub fn checked_sub(&self, previous: &Self) -> Option<Self> {
        Some(Self {
            input_tokens_total: self
                .input_tokens_total
                .checked_sub(previous.input_tokens_total)?,
            input_tokens_uncached: self
                .input_tokens_uncached
                .checked_sub(previous.input_tokens_uncached)?,
            input_tokens_cached: self
                .input_tokens_cached
                .checked_sub(previous.input_tokens_cached)?,
            cache_write_5m_tokens: self
                .cache_write_5m_tokens
                .checked_sub(previous.cache_write_5m_tokens)?,
            cache_write_1h_tokens: self
                .cache_write_1h_tokens
                .checked_sub(previous.cache_write_1h_tokens)?,
            cache_write_unknown_tokens: self
                .cache_write_unknown_tokens
                .checked_sub(previous.cache_write_unknown_tokens)?,
            output_tokens_total: self
                .output_tokens_total
                .checked_sub(previous.output_tokens_total)?,
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .checked_sub(previous.reasoning_output_tokens)?,
            web_search_requests: self
                .web_search_requests
                .checked_sub(previous.web_search_requests)?,
            web_fetch_requests: self
                .web_fetch_requests
                .checked_sub(previous.web_fetch_requests)?,
        })
    }

    pub fn is_zero(&self) -> bool {
        self == &Self::default()
    }
}

/// Provenance for a pricing-relevant dimension persisted with an observation.
/// A present value without this metadata came from a legacy record and is
/// treated as source-observed by the pricing layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DimensionValueProvenance {
    SourceObserved,
    CurrentProfileInferred,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PricingDimensions {
    pub provider_request_id: Option<String>,
    pub auth_mode: Option<String>,
    pub auth_mode_provenance: Option<DimensionValueProvenance>,
    pub provider_route: Option<String>,
    pub provider_route_provenance: Option<DimensionValueProvenance>,
    pub service_tier: Option<String>,
    pub service_tier_provenance: Option<DimensionValueProvenance>,
    pub speed: Option<String>,
    pub speed_provenance: Option<DimensionValueProvenance>,
    pub inference_geo: Option<String>,
    pub inference_geo_provenance: Option<DimensionValueProvenance>,
    pub context_window: Option<u64>,
    pub cache_write_data_complete: Option<bool>,
    /// Whether cached/write input subsets were no larger than the inclusive
    /// input counter, which is required for conservative reclassification.
    pub input_subset_accounting_consistent: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageObservation {
    pub event_key: String,
    pub client: Client,
    pub session_id: String,
    pub provider_message_id: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub raw_model: String,
    pub provider: String,
    pub usage: UsageVector,
    pub dimensions: PricingDimensions,
    pub quality: UsageQuality,
    pub coverage: CoverageStatus,
    pub source_locator: String,
    pub parser_version: String,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl UsageObservation {
    pub fn canonical_event_id(&self) -> String {
        crate::model::stable_id(&[self.client.as_str(), &self.event_key])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalEvent {
    pub event_id: String,
    pub event_key: String,
    pub client: Client,
    pub session_id: String,
    pub provider_message_id: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub raw_model: String,
    pub provider: String,
    pub usage: UsageVector,
    pub dimensions: PricingDimensions,
    pub quality: UsageQuality,
    pub coverage: CoverageStatus,
    pub source_count: u64,
    pub warnings: Vec<String>,
}

/// Describes what the ledger can truthfully say about a client's local history.
///
/// This is deliberately separate from [`CoverageStatus`], which describes the
/// completeness of the counters on one usage observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageWindowStatus {
    NoSources,
    NoObservations,
    ObservedWindow,
}

impl CoverageWindowStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSources => "no_sources",
            Self::NoObservations => "no_observations",
            Self::ObservedWindow => "observed_window",
        }
    }
}

/// A privacy-safe pointer to one edge of the observed canonical-event window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageEventBoundary {
    pub event_id: String,
    pub occurred_at: DateTime<Utc>,
}

/// The most recent successful source update for one client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuccessfulSourceScan {
    pub completed_at: DateTime<Utc>,
    pub status: String,
}

/// Coverage and ingestion health for one supported client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCoverageSnapshot {
    pub client: Client,
    pub window_status: CoverageWindowStatus,
    pub source_count: u64,
    pub observation_count: u64,
    pub canonical_event_count: u64,
    pub warning_count: u64,
    pub last_successful_source_scan: Option<SuccessfulSourceScan>,
    pub earliest_canonical_event: Option<CoverageEventBoundary>,
    pub latest_canonical_event: Option<CoverageEventBoundary>,
}

/// Metadata for a ledger scan run. No source paths or warning messages are
/// included because coverage output is also a privacy boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanRunSnapshot {
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    /// The instant at which the scanner completed its final source
    /// revalidation. `None` is retained for legacy or interrupted scan rows.
    pub as_of: Option<DateTime<Utc>>,
    pub mode: String,
    pub status: String,
    pub source_count: u64,
    pub observation_count: u64,
    pub warning_count: u64,
    /// Sources that changed while they were parsed or between their parse and
    /// the final revalidation pass.
    pub active_or_volatile_source_count: u64,
    /// True when the snapshot may move because a source was active/volatile,
    /// or when the scan did not complete successfully.
    pub provisional: bool,
}

/// A warning rollup that intentionally exposes only a sanitized code and count.
/// `client` is `None` for warnings that were not associated with a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarningCodeCount {
    pub client: Option<Client>,
    pub code: String,
    pub count: u64,
}

/// Full coverage envelope suitable for `doctor`, reports, and JSON export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerCoverageSnapshot {
    pub generated_at: DateTime<Utc>,
    /// The revalidation boundary of `last_scan`, if a completed scan exists.
    pub as_of: Option<DateTime<Utc>>,
    pub active_or_volatile_source_count: u64,
    pub provisional: bool,
    pub last_scan: Option<ScanRunSnapshot>,
    pub clients: Vec<ClientCoverageSnapshot>,
    pub warning_counts: Vec<WarningCodeCount>,
}

/// One stored observation contributing to a canonical event. The source id is
/// pseudonymous and the locator is reduced to line/byte coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationProvenance {
    pub pseudonymous_source_id: String,
    pub source_locator: String,
    pub parser_version: String,
    pub occurred_at: DateTime<Utc>,
    pub raw_model: String,
    pub usage: UsageVector,
    pub quality: UsageQuality,
    pub coverage: CoverageStatus,
}

/// Privacy-safe provenance for all observations folded into one canonical event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventProvenance {
    pub event_id: String,
    pub client: Client,
    pub observation_count: u64,
    pub deduplicated_observation_count: u64,
    pub source_count: u64,
    pub observations: Vec<ObservationProvenance>,
}

#[derive(Debug, Clone)]
pub struct SourceSpec {
    pub path: PathBuf,
    pub client: Client,
    pub compressed: bool,
}

#[derive(Debug, Clone)]
pub struct LineRecord {
    pub line_number: u64,
    pub byte_start: u64,
    pub byte_end: u64,
    pub text: String,
}

impl LineRecord {
    pub fn locator(&self) -> String {
        format!("line {} @ byte {}", self.line_number, self.byte_start)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanWarning {
    pub code: String,
    pub message: String,
    pub locator: Option<String>,
}

impl ScanWarning {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            locator: None,
        }
    }

    pub fn at(mut self, locator: impl Into<String>) -> Self {
        self.locator = Some(locator.into());
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParseBatch {
    pub observations: Vec<UsageObservation>,
    pub warnings: Vec<ScanWarning>,
    pub next_state: Value,
}

pub fn stable_id(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    format!("evt_{}", &hex::encode(digest)[..24])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_ids_are_deterministic_and_delimited() {
        assert_eq!(stable_id(&["ab", "c"]), stable_id(&["ab", "c"]));
        assert_ne!(stable_id(&["ab", "c"]), stable_id(&["a", "bc"]));
    }

    #[test]
    fn componentwise_sub_rejects_resets() {
        let current = UsageVector {
            input_tokens_total: 10,
            ..Default::default()
        };
        let previous = UsageVector {
            input_tokens_total: 11,
            ..Default::default()
        };
        assert!(current.checked_sub(&previous).is_none());
    }
}
