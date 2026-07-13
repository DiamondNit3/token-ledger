//! Effective-dated, reproducible model pricing.
//!
//! The pricing layer deliberately models unknown and partially priced usage as
//! states rather than as numeric zero. Catalog decimals are encoded as JSON
//! strings and every calculation uses [`Decimal`].

use std::collections::{BTreeMap, BTreeSet, HashSet};
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::config::PricingDimensionOverride;
use crate::model::{
    CanonicalEvent, Client, DimensionValueProvenance, PricingDimensions, UsageObservation,
    UsageVector,
};

const CATALOG_SCHEMA_VERSION: u32 = 1;
const OFFICIAL_MANIFEST_SCHEMA_VERSION: u32 = 1;
const MAX_CATALOG_COLLECTION_ENTRIES: usize = 2_048;
const MAX_CATALOG_DECIMAL_MAGNITUDE: u64 = 1_000_000_000;
const MAX_PRICING_RESULT_MAGNITUDE: u64 = 1_000_000_000_000_000_000;
const BUNDLED_CATALOG: &[u8] = include_bytes!("../assets/prices.json");
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A decimal serialized as a string so JSON parsing never passes through an
/// IEEE-754 floating-point value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExactDecimal(pub Decimal);

impl ExactDecimal {
    pub fn value(self) -> Decimal {
        self.0
    }
}

impl Serialize for ExactDecimal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for ExactDecimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Decimal::from_str(&raw)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error)]
pub enum PricingError {
    #[error("failed to read price catalog {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write price catalog {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid price catalog JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("price catalog failed verification: {0}")]
    Verification(String),
    #[error("catalog checksum mismatch: expected {expected}, calculated {calculated}")]
    ChecksumMismatch {
        expected: String,
        calculated: String,
    },
    #[error("catalog update was rejected: {0}")]
    UpdateRejected(String),
    #[error("invalid pricing override configuration: {0}")]
    InvalidOverride(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateKind {
    UsdApiEquivalent,
    CodexCredits,
}

impl RateKind {
    pub fn unit_name(self) -> &'static str {
        match self {
            Self::UsdApiEquivalent => "USD",
            Self::CodexCredits => "Codex credits",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSource {
    pub id: String,
    pub title: String,
    pub url: String,
    pub retrieved_at: DateTime<Utc>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveInterval {
    pub effective_from: DateTime<Utc>,
    #[serde(default)]
    pub effective_to: Option<DateTime<Utc>>,
}

impl EffectiveInterval {
    fn contains(&self, value: DateTime<Utc>) -> bool {
        value >= self.effective_from && self.effective_to.is_none_or(|end| value < end)
    }

    fn overlaps(&self, other: &Self) -> bool {
        let self_before_other_end = other
            .effective_to
            .is_none_or(|end| self.effective_from < end);
        let other_before_self_end = self
            .effective_to
            .is_none_or(|end| other.effective_from < end);
        self_before_other_end && other_before_self_end
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAlias {
    pub id: String,
    pub provider: String,
    pub raw_model: String,
    pub canonical_model: String,
    #[serde(flatten)]
    pub interval: EffectiveInterval,
    #[serde(default)]
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenRates {
    #[serde(default)]
    pub input: Option<ExactDecimal>,
    #[serde(default)]
    pub cache_read: Option<ExactDecimal>,
    /// Generic/unknown-TTL cache write rate (for example OpenAI's 30m write).
    #[serde(default)]
    pub cache_write: Option<ExactDecimal>,
    #[serde(default)]
    pub cache_write_5m: Option<ExactDecimal>,
    #[serde(default)]
    pub cache_write_1h: Option<ExactDecimal>,
    #[serde(default)]
    pub output: Option<ExactDecimal>,
}

impl TokenRates {
    fn any_cache_write_rate(&self) -> bool {
        self.cache_write.is_some() || self.cache_write_5m.is_some() || self.cache_write_1h.is_some()
    }

    fn iter_named(&self) -> [(&'static str, Option<ExactDecimal>); 6] {
        [
            ("input", self.input),
            ("cache_read", self.cache_read),
            ("cache_write", self.cache_write),
            ("cache_write_5m", self.cache_write_5m),
            ("cache_write_1h", self.cache_write_1h),
            ("output", self.output),
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUnitRate {
    pub rate: ExactDecimal,
    pub per_units: u64,
    #[serde(default)]
    pub explicit_free: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionPolicy {
    pub allowed: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateRule {
    pub id: String,
    pub provider: String,
    pub canonical_model: String,
    pub kind: RateKind,
    pub unit_scale: u64,
    #[serde(flatten)]
    pub interval: EffectiveInterval,
    #[serde(default)]
    pub min_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_input_tokens_exclusive: Option<u64>,
    #[serde(default)]
    pub dimension_policies: BTreeMap<String, DimensionPolicy>,
    pub rates: TokenRates,
    #[serde(default)]
    pub tool_rates: BTreeMap<String, ToolUnitRate>,
    /// If true, an absent/incomplete cache-write counter makes the result partial.
    #[serde(default)]
    pub cache_write_observation_required: bool,
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

impl RateRule {
    fn context_matches(&self, total_input_tokens: u64) -> bool {
        self.min_input_tokens
            .is_none_or(|min| total_input_tokens >= min)
            && self
                .max_input_tokens_exclusive
                .is_none_or(|max| total_input_tokens < max)
    }

    fn context_overlaps(&self, other: &Self) -> bool {
        let a_min = self.min_input_tokens.unwrap_or(0);
        let b_min = other.min_input_tokens.unwrap_or(0);
        let a_before_b_end = other
            .max_input_tokens_exclusive
            .is_none_or(|end| a_min < end);
        let b_before_a_end = self
            .max_input_tokens_exclusive
            .is_none_or(|end| b_min < end);
        a_before_b_end && b_before_a_end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModifierScope {
    TokenRates,
    ToolRates,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateModifier {
    pub id: String,
    pub provider: String,
    pub canonical_model: String,
    pub kind: RateKind,
    #[serde(flatten)]
    pub interval: EffectiveInterval,
    pub selectors: BTreeMap<String, String>,
    pub multiplier: ExactDecimal,
    pub scope: ModifierScope,
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogDocument {
    schema_version: u32,
    revision: String,
    published_at: DateTime<Utc>,
    verified_at: DateTime<Utc>,
    stale_after_days: u32,
    #[serde(default)]
    coverage_note: Option<String>,
    sources: Vec<CatalogSource>,
    aliases: Vec<ModelAlias>,
    rates: Vec<RateRule>,
    #[serde(default)]
    modifiers: Vec<RateModifier>,
}

/// Parsed catalog plus the digest of the exact bytes it came from.
#[derive(Debug, Clone)]
pub struct PriceCatalog {
    /// Compatibility-facing catalog version; identical to the document revision.
    pub version: String,
    document: CatalogDocument,
    sha256: String,
    raw_bytes: Vec<u8>,
}

impl PriceCatalog {
    pub fn parse(bytes: &[u8]) -> Result<Self, PricingError> {
        let document: CatalogDocument = serde_json::from_slice(bytes)?;
        let version = document.revision.clone();
        Ok(Self {
            version,
            document,
            sha256: sha256_hex(bytes),
            raw_bytes: bytes.to_vec(),
        })
    }

    pub fn revision(&self) -> &str {
        &self.document.revision
    }

    pub fn schema_version(&self) -> u32 {
        self.document.schema_version
    }

    pub fn published_at(&self) -> DateTime<Utc> {
        self.document.published_at
    }

    pub fn verified_at(&self) -> DateTime<Utc> {
        self.document.verified_at
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn sources(&self) -> &[CatalogSource] {
        &self.document.sources
    }

    pub fn aliases(&self) -> &[ModelAlias] {
        &self.document.aliases
    }

    pub fn rates(&self) -> &[RateRule] {
        &self.document.rates
    }

    pub fn coverage_note(&self) -> Option<&str> {
        self.document.coverage_note.as_deref()
    }

    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw_bytes
    }

    pub fn verify(&self) -> VerificationReport {
        verify_document(&self.document)
    }
}

impl Serialize for PriceCatalog {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.document.serialize(serializer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationIssue {
    pub severity: VerificationSeverity,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerificationReport {
    pub valid: bool,
    pub issues: Vec<VerificationIssue>,
}

impl VerificationReport {
    pub fn is_valid(&self) -> bool {
        self.valid
    }

    pub fn error_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.severity == VerificationSeverity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.issues.len().saturating_sub(self.error_count())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFreshness {
    Fresh,
    Stale,
    FutureDated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogStatus {
    pub revision: String,
    pub schema_version: u32,
    pub sha256: String,
    pub published_at: DateTime<Utc>,
    pub verified_at: DateTime<Utc>,
    pub stale_at: DateTime<Utc>,
    pub freshness: CatalogFreshness,
    pub source_count: usize,
    pub alias_count: usize,
    pub rate_count: usize,
    pub modifier_count: usize,
    pub verification: VerificationReport,
}

/// One immutable, revision-named catalog copy retained beside the active file.
/// The checksum is part of the filename so two byte-distinct documents can
/// never silently overwrite one another even if a publisher reuses a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedCatalogRevision {
    pub revision: String,
    pub sha256: String,
    pub path: PathBuf,
}

/// Durable installation result. `retained_revisions` includes the newly
/// installed catalog and, when available, the previously active revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogInstallReceipt {
    pub active_path: PathBuf,
    pub installed_revision: String,
    pub installed_sha256: String,
    pub history_dir: PathBuf,
    pub retained_revisions: Vec<RetainedCatalogRevision>,
}

/// Human-auditable metadata carried by a pinned official update manifest.
/// This is checksum authentication, not a cryptographic signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEvidenceMetadata {
    /// Catalog source IDs reviewed for this release. Every ID must exist in
    /// the referenced catalog.
    pub source_ids: Vec<String>,
    /// Concise description of the review performed for this release.
    pub note: String,
}

/// Versioned checksum manifest for an explicitly requested official update.
/// The manifest bytes themselves must be pinned by a trusted SHA-256 value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfficialCatalogManifest {
    pub schema_version: u32,
    pub catalog_revision: String,
    /// HTTPS URL or a path relative to the local manifest file.
    pub catalog_reference: String,
    pub catalog_sha256: String,
    pub published_at: DateTime<Utc>,
    pub verified_at: DateTime<Utc>,
    pub evidence: ManifestEvidenceMetadata,
}

impl OfficialCatalogManifest {
    /// Verify the trusted manifest pin before parsing or using any field.
    pub fn parse_pinned(bytes: &[u8], expected_sha256: &str) -> Result<Self, PricingError> {
        verify_sha256(bytes, expected_sha256)?;
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), PricingError> {
        if self.schema_version != OFFICIAL_MANIFEST_SCHEMA_VERSION {
            return Err(PricingError::Verification(format!(
                "official manifest schema version {} is unsupported; expected {}",
                self.schema_version, OFFICIAL_MANIFEST_SCHEMA_VERSION
            )));
        }
        if self.catalog_revision.trim().is_empty() {
            return Err(PricingError::Verification(
                "official manifest catalog_revision cannot be empty".to_string(),
            ));
        }
        if self.catalog_reference.trim().is_empty() {
            return Err(PricingError::Verification(
                "official manifest catalog_reference cannot be empty".to_string(),
            ));
        }
        if self.catalog_reference.starts_with("http://") {
            return Err(PricingError::Verification(
                "official manifest catalog_reference must use HTTPS or a local file".to_string(),
            ));
        }
        validate_sha256_text(&self.catalog_sha256)?;
        if self.verified_at < self.published_at {
            return Err(PricingError::Verification(
                "official manifest verified_at precedes published_at".to_string(),
            ));
        }
        if self.evidence.source_ids.is_empty()
            || self
                .evidence
                .source_ids
                .iter()
                .any(|value| value.trim().is_empty())
        {
            return Err(PricingError::Verification(
                "official manifest evidence.source_ids must contain reviewed catalog source IDs"
                    .to_string(),
            ));
        }
        if self.evidence.note.trim().is_empty() {
            return Err(PricingError::Verification(
                "official manifest evidence.note cannot be empty".to_string(),
            ));
        }
        Ok(())
    }

    /// Bind a checksum-verified catalog to all manifest metadata.
    pub fn verify_catalog(&self, bytes: &[u8]) -> Result<PriceCatalog, PricingError> {
        verify_sha256(bytes, &self.catalog_sha256)?;
        let catalog = PriceCatalog::parse(bytes)?;
        require_verified(&catalog)?;
        if catalog.revision() != self.catalog_revision {
            return Err(PricingError::Verification(format!(
                "manifest revision '{}' does not match catalog revision '{}'",
                self.catalog_revision,
                catalog.revision()
            )));
        }
        if catalog.published_at() != self.published_at {
            return Err(PricingError::Verification(format!(
                "manifest published_at {} does not match catalog published_at {}",
                self.published_at,
                catalog.published_at()
            )));
        }
        if catalog.verified_at() != self.verified_at {
            return Err(PricingError::Verification(format!(
                "manifest verified_at {} does not match catalog verified_at {}",
                self.verified_at,
                catalog.verified_at()
            )));
        }
        let catalog_source_ids = catalog
            .sources()
            .iter()
            .map(|source| source.id.as_str())
            .collect::<HashSet<_>>();
        for source_id in &self.evidence.source_ids {
            if !catalog_source_ids.contains(source_id.as_str()) {
                return Err(PricingError::Verification(format!(
                    "manifest evidence source ID '{source_id}' is absent from the catalog"
                )));
            }
        }
        Ok(catalog)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogCandidateRelation {
    Current,
    Newer,
    Downgrade,
    RevisionConflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCheckResult {
    pub active_revision: String,
    pub active_sha256: String,
    pub active_published_at: DateTime<Utc>,
    pub candidate_revision: String,
    pub candidate_sha256: String,
    pub candidate_published_at: DateTime<Utc>,
    pub relation: CatalogCandidateRelation,
    pub update_allowed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCollectionDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogDiff {
    pub from_revision: String,
    pub from_sha256: String,
    pub to_revision: String,
    pub to_sha256: String,
    pub metadata_changed: Vec<String>,
    pub sources: CatalogCollectionDiff,
    pub aliases: CatalogCollectionDiff,
    pub rates: CatalogCollectionDiff,
    pub modifiers: CatalogCollectionDiff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateStatus {
    Priced,
    Partial,
    Unpriced,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingPriceComponent {
    pub rate_kind: RateKind,
    pub component: String,
    pub quantity: Option<u64>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingEvidence {
    pub record_id: String,
    pub record_type: String,
    pub effective_from: DateTime<Utc>,
    pub effective_to: Option<DateTime<Utc>>,
    pub source_ids: Vec<String>,
    pub sources: Vec<CatalogSource>,
}

/// Why a pricing dimension was supplied even though it was not established by
/// the source event itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssumptionProvenance {
    /// The value was persisted in the source event itself.
    SourceObserved,
    /// The value came from the current local profile and may not describe the
    /// historical session being priced.
    CurrentProfileInferred,
    /// The catalog's documented default, used by the backwards-compatible
    /// single-estimate API when the source value is absent.
    CatalogDefault,
    /// One of the catalog's documented allowed values, explored by the range
    /// API because the source value was missing or explicitly unresolved.
    CatalogAllowedScenario,
    /// A bounded value explicitly attested by the user in configuration.
    UserAttestedOverride,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingDimensionConfidence {
    Observed,
    Attested,
    Inferred,
    Scenario,
}

/// Serializable provenance for a pricing assumption. Catalog-derived entries
/// carry official catalog sources; user attestations carry their bounded
/// interval, timestamp, identifier, and note instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingAssumptionEvidence {
    pub rate_kind: RateKind,
    pub dimension: String,
    pub value: String,
    pub provenance: AssumptionProvenance,
    pub confidence: PricingDimensionConfidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_to: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attested_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<CatalogSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceRangeStatus {
    Exact,
    Bounded,
    Partial,
    Unpriced,
}

/// Independent confidence state for one priced measure. `Unavailable` means
/// the measure does not apply (for example Codex credits on an API-key event),
/// while `Unpriced` means it applies but the catalog cannot price it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasureStatus {
    Exact,
    Bounded,
    Partial,
    Unpriced,
    #[default]
    Unavailable,
}

/// One fully specified catalog scenario considered when building a range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingScenarioEstimate {
    pub dimensions: BTreeMap<String, String>,
    pub status: EstimateStatus,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub amount: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub known_amount: Option<Decimal>,
    /// Finite conservative endpoint when an incomplete source counter can be
    /// bounded by an accounting invariant.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub upper_amount: Option<Decimal>,
    pub matched_rate_ids: Vec<String>,
    pub assumptions: Vec<PricingAssumptionEvidence>,
    pub missing_components: Vec<MissingPriceComponent>,
    pub explanation: Vec<String>,
}

/// Exact-decimal bounds for one independently priced measure. A bounded range
/// has both endpoints. A partial range has no finite upper endpoint because at
/// least one billable component remains unknown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioPriceRange {
    pub rate_kind: RateKind,
    pub unit_name: String,
    pub status: PriceRangeStatus,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub lower_bound: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub upper_bound: Option<Decimal>,
    pub scenarios: Vec<PricingScenarioEstimate>,
}

/// Scenario-aware counterpart to [`CostEstimate`]. USD and provider units are
/// deliberately independent because one may be bounded while the other is
/// exact, partial, or inapplicable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioCostEstimate {
    pub api_equivalent_usd: ScenarioPriceRange,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_units: Option<ScenarioPriceRange>,
    pub catalog_version: String,
}

/// v2 estimate for one measure. Bounds use exact decimal arithmetic and never
/// turn an unknown component into numeric zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingMeasureEstimate {
    pub rate_kind: RateKind,
    pub unit_name: String,
    pub status: MeasureStatus,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub lower_bound: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub upper_bound: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dimension_evidence: Vec<PricingAssumptionEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_components: Vec<MissingPriceComponent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explanation: Vec<String>,
}

impl PricingMeasureEstimate {
    fn unavailable(kind: RateKind, reason: impl Into<String>) -> Self {
        Self {
            rate_kind: kind,
            unit_name: kind.unit_name().to_string(),
            status: MeasureStatus::Unavailable,
            lower_bound: None,
            upper_bound: None,
            dimension_evidence: Vec::new(),
            missing_components: Vec::new(),
            explanation: vec![reason.into()],
        }
    }
}

/// Combined public estimate for one canonical event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostEstimate {
    pub status: EstimateStatus,
    /// Independent v2 API-list-price measure. Prefer this over the legacy
    /// scalar fields below when presenting confidence or bounds.
    pub api_equivalent_usd_measure: PricingMeasureEstimate,
    /// Independent v2 provider-unit measure. It is present even when the
    /// measure is unavailable so callers can distinguish N/A from unpriced.
    pub provider_unit_measure: PricingMeasureEstimate,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub api_equivalent_usd: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub provider_units: Option<Decimal>,
    pub provider_unit_name: Option<String>,
    /// Known subtotal when the corresponding exact amount is unavailable.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub known_api_equivalent_usd: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub known_provider_units: Option<Decimal>,
    pub catalog_version: String,
    pub matched_rate_ids: Vec<String>,
    pub pricing_evidence: Vec<PricingEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pricing_assumptions: Vec<PricingAssumptionEvidence>,
    pub missing_components: Vec<MissingPriceComponent>,
    pub explanation: Vec<String>,
}

#[derive(Debug, Clone)]
struct KindEstimate {
    status: EstimateStatus,
    exact: Option<Decimal>,
    known: Option<Decimal>,
    upper: Option<Decimal>,
    matched_rate_ids: Vec<String>,
    assumptions: Vec<PricingAssumptionEvidence>,
    missing: Vec<MissingPriceComponent>,
    explanation: Vec<String>,
}

impl KindEstimate {
    fn unpriced(kind: RateKind, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            status: EstimateStatus::Unpriced,
            exact: None,
            known: None,
            upper: None,
            matched_rate_ids: Vec::new(),
            assumptions: Vec::new(),
            missing: vec![MissingPriceComponent {
                rate_kind: kind,
                component: "rate_rule".to_string(),
                quantity: None,
                reason: reason.clone(),
            }],
            explanation: vec![reason],
        }
    }
}

/// Pricing catalog resolver and exact-decimal calculator.
#[derive(Debug, Clone)]
pub struct PricingEngine {
    catalog: PriceCatalog,
    dimension_overrides: Vec<PricingDimensionOverride>,
}

impl PricingEngine {
    pub fn bundled() -> Result<Self, PricingError> {
        Self::from_bytes(BUNDLED_CATALOG)
    }

    pub fn load(path: &Path) -> Result<Self, PricingError> {
        let bytes = fs::read(path).map_err(|source| PricingError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_bytes(&bytes)
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, PricingError> {
        let catalog = PriceCatalog::parse(bytes)?;
        require_verified(&catalog)?;
        Ok(Self {
            catalog,
            dimension_overrides: Vec::new(),
        })
    }

    /// Attach validated, time-bounded user attestations. Callers can pass
    /// `config.pricing_dimension_overrides.clone()` and retain the resulting
    /// engine for all report calculations.
    pub fn with_dimension_overrides(
        mut self,
        dimension_overrides: Vec<PricingDimensionOverride>,
    ) -> Result<Self, PricingError> {
        validate_dimension_overrides(&dimension_overrides)?;
        self.validate_overrides_against_catalog(&dimension_overrides)?;
        self.dimension_overrides = dimension_overrides;
        Ok(self)
    }

    pub fn dimension_overrides(&self) -> &[PricingDimensionOverride] {
        &self.dimension_overrides
    }

    fn validate_overrides_against_catalog(
        &self,
        dimension_overrides: &[PricingDimensionOverride],
    ) -> Result<(), PricingError> {
        for override_value in dimension_overrides {
            let interval = EffectiveInterval {
                effective_from: override_value.effective_from,
                effective_to: Some(override_value.effective_to),
            };
            let policies: Vec<&DimensionPolicy> = self
                .catalog
                .document
                .rates
                .iter()
                .filter(|rule| {
                    rule.provider.eq_ignore_ascii_case(&override_value.provider)
                        && rule.canonical_model == override_value.canonical_model
                        && rule.interval.overlaps(&interval)
                })
                .filter_map(|rule| {
                    rule.dimension_policies
                        .get(&override_value.dimension.trim().to_ascii_lowercase())
                })
                .collect();
            if policies.is_empty() {
                return Err(PricingError::InvalidOverride(format!(
                    "override '{}' does not target a documented pricing dimension for {}/{} during its effective interval",
                    override_value.id, override_value.provider, override_value.canonical_model
                )));
            }
            for policy in policies {
                resolve_explicit_dimension(
                    &override_value.dimension,
                    policy,
                    &override_value.value,
                )
                .map_err(|reason| {
                    PricingError::InvalidOverride(format!(
                        "override '{}': {reason}",
                        override_value.id
                    ))
                })?;
            }
        }
        Ok(())
    }

    fn active_dimension_override(
        &self,
        provider: &str,
        canonical_model: &str,
        dimension: &str,
        occurred_at: DateTime<Utc>,
    ) -> Option<&PricingDimensionOverride> {
        self.dimension_overrides.iter().find(|override_value| {
            override_value.provider.eq_ignore_ascii_case(provider)
                && override_value.canonical_model == canonical_model
                && override_value.dimension.eq_ignore_ascii_case(dimension)
                && override_value.contains(occurred_at)
        })
    }

    fn catalog_assumption_evidence(
        &self,
        rate_kind: RateKind,
        dimension: &str,
        value: &str,
        provenance: AssumptionProvenance,
        interval: &EffectiveInterval,
        source_ids: &[String],
    ) -> PricingAssumptionEvidence {
        PricingAssumptionEvidence {
            rate_kind,
            dimension: dimension.to_string(),
            value: value.to_string(),
            provenance,
            confidence: PricingDimensionConfidence::Scenario,
            override_id: None,
            effective_from: Some(interval.effective_from),
            effective_to: interval.effective_to,
            attested_at: None,
            note: None,
            source_ids: source_ids.to_vec(),
            sources: self.sources_for_ids(source_ids),
        }
    }

    fn sources_for_ids(&self, source_ids: &[String]) -> Vec<CatalogSource> {
        source_ids
            .iter()
            .filter_map(|id| {
                self.catalog
                    .document
                    .sources
                    .iter()
                    .find(|source| source.id == *id)
                    .cloned()
            })
            .collect()
    }

    pub fn catalog(&self) -> &PriceCatalog {
        &self.catalog
    }

    pub fn verify(&self) -> VerificationReport {
        self.catalog.verify()
    }

    pub fn status(&self) -> CatalogStatus {
        self.status_at(Utc::now())
    }

    pub fn status_at(&self, now: DateTime<Utc>) -> CatalogStatus {
        let stale_at = self.catalog.document.verified_at
            + Duration::days(i64::from(self.catalog.document.stale_after_days));
        let freshness = if now < self.catalog.document.published_at {
            CatalogFreshness::FutureDated
        } else if now > stale_at {
            CatalogFreshness::Stale
        } else {
            CatalogFreshness::Fresh
        };
        CatalogStatus {
            revision: self.catalog.document.revision.clone(),
            schema_version: self.catalog.document.schema_version,
            sha256: self.catalog.sha256.clone(),
            published_at: self.catalog.document.published_at,
            verified_at: self.catalog.document.verified_at,
            stale_at,
            freshness,
            source_count: self.catalog.document.sources.len(),
            alias_count: self.catalog.document.aliases.len(),
            rate_count: self.catalog.document.rates.len(),
            modifier_count: self.catalog.document.modifiers.len(),
            verification: self.verify(),
        }
    }

    pub fn estimate_event(&self, event: &CanonicalEvent) -> CostEstimate {
        let dimensions = dimensions_with_legacy_provenance(&event.dimensions, &event.warnings);
        self.estimate_fields(
            event.client,
            &event.provider,
            &event.raw_model,
            event.occurred_at,
            &event.usage,
            &dimensions,
        )
    }

    pub fn estimate_observation(&self, event: &UsageObservation) -> CostEstimate {
        let dimensions = dimensions_with_legacy_provenance(&event.dimensions, &event.warnings);
        self.estimate_fields(
            event.client,
            &event.provider,
            &event.raw_model,
            event.occurred_at,
            &event.usage,
            &dimensions,
        )
    }

    /// Enumerate every documented pricing scenario needed to resolve missing
    /// or explicitly unavailable dimensions and return exact decimal bounds.
    pub fn estimate_event_range(&self, event: &CanonicalEvent) -> ScenarioCostEstimate {
        let dimensions = dimensions_with_legacy_provenance(&event.dimensions, &event.warnings);
        self.estimate_fields_range(
            event.client,
            &event.provider,
            &event.raw_model,
            event.occurred_at,
            &event.usage,
            &dimensions,
        )
    }

    pub fn estimate_observation_range(&self, event: &UsageObservation) -> ScenarioCostEstimate {
        let dimensions = dimensions_with_legacy_provenance(&event.dimensions, &event.warnings);
        self.estimate_fields_range(
            event.client,
            &event.provider,
            &event.raw_model,
            event.occurred_at,
            &event.usage,
            &dimensions,
        )
    }

    fn estimate_fields(
        &self,
        client: Client,
        provider: &str,
        raw_model: &str,
        occurred_at: DateTime<Utc>,
        usage: &UsageVector,
        dimensions: &PricingDimensions,
    ) -> CostEstimate {
        let usd = self.estimate_kind(
            provider,
            raw_model,
            occurred_at,
            usage,
            dimensions,
            RateKind::UsdApiEquivalent,
        );
        let usd_range = self.estimate_kind_range(
            provider,
            raw_model,
            occurred_at,
            usage,
            dimensions,
            RateKind::UsdApiEquivalent,
        );
        let auth_override = self
            .resolve_model(
                &provider.trim().to_ascii_lowercase(),
                raw_model,
                occurred_at,
            )
            .and_then(|(canonical, _)| {
                self.active_dimension_override(provider, &canonical, "auth_mode", occurred_at)
            });
        let auth_uncertain = auth_override.is_none()
            && (dimensions.auth_mode.is_none()
                || dimensions.auth_mode_provenance
                    == Some(DimensionValueProvenance::CurrentProfileInferred));
        let api_key_route = !auth_uncertain
            && self
                .effective_auth_mode(provider, raw_model, occurred_at, dimensions)
                .as_deref()
                .is_some_and(is_api_key_auth_mode);
        let credits = if client == Client::OpenaiCodex && !api_key_route {
            Some(self.estimate_kind(
                provider,
                raw_model,
                occurred_at,
                usage,
                dimensions,
                RateKind::CodexCredits,
            ))
        } else {
            None
        };

        let mut credit_range_dimensions = dimensions.clone();
        if auth_uncertain {
            credit_range_dimensions.auth_mode = None;
        }
        let credits_range = (client == Client::OpenaiCodex && !api_key_route).then(|| {
            self.estimate_kind_range(
                provider,
                raw_model,
                occurred_at,
                usage,
                &credit_range_dimensions,
                RateKind::CodexCredits,
            )
        });
        let mut api_equivalent_usd_measure =
            self.measure_from_range(provider, raw_model, occurred_at, dimensions, usd_range);
        api_equivalent_usd_measure.explanation.insert(
            0,
            "API-equivalent USD is a public list-price estimate, not an invoice or subscription charge."
                .to_string(),
        );
        let mut provider_unit_measure = match credits_range {
            Some(range) => {
                self.measure_from_range(provider, raw_model, occurred_at, dimensions, range)
            }
            None => PricingMeasureEstimate::unavailable(
                RateKind::CodexCredits,
                if client == Client::OpenaiCodex && api_key_route {
                    "Codex credits do not apply to API-key authenticated events."
                } else {
                    "Codex credits do not apply to this client."
                },
            ),
        };
        if auth_uncertain
            && matches!(
                provider_unit_measure.status,
                MeasureStatus::Exact | MeasureStatus::Bounded
            )
        {
            provider_unit_measure.status = MeasureStatus::Bounded;
            provider_unit_measure.lower_bound = Some(Decimal::ZERO);
            provider_unit_measure.explanation.push(
                "Historical authentication is not source-observed; credits are bounded from zero (API-key route) to the documented ChatGPT-route amount."
                    .to_string(),
            );
        }

        let mut legacy_statuses = vec![usd.status];
        if let Some(value) = &credits {
            legacy_statuses.push(value.status);
        }
        let mut status = if legacy_statuses
            .iter()
            .all(|value| *value == EstimateStatus::Priced)
        {
            EstimateStatus::Priced
        } else if legacy_statuses
            .iter()
            .all(|value| *value == EstimateStatus::Unpriced)
        {
            EstimateStatus::Unpriced
        } else {
            EstimateStatus::Partial
        };
        if auth_uncertain && credits.is_some() {
            status = EstimateStatus::Partial;
        }

        let mut matched_rate_ids = usd.matched_rate_ids.clone();
        let mut pricing_assumptions = usd.assumptions.clone();
        let mut missing_components = usd.missing.clone();
        let mut explanation = usd.explanation.clone();
        explanation.insert(
            0,
            "API-equivalent USD is a public list-price estimate, not an invoice or subscription charge."
                .to_string(),
        );
        if let Some(value) = &credits {
            matched_rate_ids.extend(value.matched_rate_ids.clone());
            pricing_assumptions.extend(value.assumptions.clone());
            missing_components.extend(value.missing.clone());
            explanation.extend(value.explanation.clone());
            explanation
                .push("Codex credits are provider units and are not converted to USD.".to_string());
        } else if client == Client::OpenaiCodex && api_key_route {
            explanation.push(
                "Codex credits do not apply because this event used API-key authentication; API-equivalent USD remains the relevant estimate."
                    .to_string(),
            );
        }
        if auth_uncertain && credits.is_some() {
            missing_components.push(MissingPriceComponent {
                rate_kind: RateKind::CodexCredits,
                component: "auth_mode_provenance".to_string(),
                quantity: None,
                reason: "Historical Codex authentication was not observed in the session; the current profile cannot establish exact historical credits."
                    .to_string(),
            });
            explanation.push(
                "The legacy exact credit scalar is withheld because historical authentication is inferred or missing."
                    .to_string(),
            );
        }
        matched_rate_ids.sort();
        matched_rate_ids.dedup();
        let pricing_evidence =
            self.pricing_evidence(provider, raw_model, occurred_at, &matched_rate_ids);

        CostEstimate {
            status,
            api_equivalent_usd_measure,
            provider_unit_measure,
            api_equivalent_usd: usd.exact,
            provider_units: (!auth_uncertain)
                .then(|| credits.as_ref().and_then(|value| value.exact))
                .flatten(),
            provider_unit_name: credits.as_ref().map(|_| "Codex credits".to_string()),
            known_api_equivalent_usd: usd.known,
            known_provider_units: (!auth_uncertain)
                .then(|| credits.as_ref().and_then(|value| value.known))
                .flatten(),
            catalog_version: self.catalog.document.revision.clone(),
            matched_rate_ids,
            pricing_evidence,
            pricing_assumptions,
            missing_components,
            explanation,
        }
    }

    fn estimate_fields_range(
        &self,
        client: Client,
        provider: &str,
        raw_model: &str,
        occurred_at: DateTime<Utc>,
        usage: &UsageVector,
        dimensions: &PricingDimensions,
    ) -> ScenarioCostEstimate {
        let api_equivalent_usd = self.estimate_kind_range(
            provider,
            raw_model,
            occurred_at,
            usage,
            dimensions,
            RateKind::UsdApiEquivalent,
        );
        let api_key_route = self
            .effective_auth_mode(provider, raw_model, occurred_at, dimensions)
            .as_deref()
            .is_some_and(is_api_key_auth_mode);
        let provider_units = (client == Client::OpenaiCodex && !api_key_route).then(|| {
            self.estimate_kind_range(
                provider,
                raw_model,
                occurred_at,
                usage,
                dimensions,
                RateKind::CodexCredits,
            )
        });
        ScenarioCostEstimate {
            api_equivalent_usd,
            provider_units,
            catalog_version: self.catalog.document.revision.clone(),
        }
    }

    fn measure_from_range(
        &self,
        provider: &str,
        raw_model: &str,
        occurred_at: DateTime<Utc>,
        dimensions: &PricingDimensions,
        range: ScenarioPriceRange,
    ) -> PricingMeasureEstimate {
        let status = match range.status {
            PriceRangeStatus::Exact => MeasureStatus::Exact,
            PriceRangeStatus::Bounded => MeasureStatus::Bounded,
            PriceRangeStatus::Partial => MeasureStatus::Partial,
            PriceRangeStatus::Unpriced => MeasureStatus::Unpriced,
        };
        let mut dimension_evidence = range
            .scenarios
            .iter()
            .flat_map(|scenario| scenario.assumptions.iter().cloned())
            .collect::<Vec<_>>();
        let relevant_dimensions = range
            .scenarios
            .iter()
            .flat_map(|scenario| scenario.dimensions.keys().cloned())
            .collect::<BTreeSet<_>>();
        let provider_normalized = provider.trim().to_ascii_lowercase();
        let canonical_model = self
            .resolve_model(&provider_normalized, raw_model, occurred_at)
            .map(|(canonical, _)| canonical);
        for name in relevant_dimensions {
            let Some((value, source)) = dimension_value_and_provenance(dimensions, &name) else {
                continue;
            };
            if is_explicitly_unresolved_dimension_value(value) {
                continue;
            }
            let overridden = canonical_model.as_deref().is_some_and(|canonical| {
                self.active_dimension_override(&provider_normalized, canonical, &name, occurred_at)
                    .is_some()
            });
            if overridden {
                continue;
            }
            let (provenance, confidence) =
                match source.unwrap_or(DimensionValueProvenance::SourceObserved) {
                    DimensionValueProvenance::SourceObserved => (
                        AssumptionProvenance::SourceObserved,
                        PricingDimensionConfidence::Observed,
                    ),
                    DimensionValueProvenance::CurrentProfileInferred => (
                        AssumptionProvenance::CurrentProfileInferred,
                        PricingDimensionConfidence::Inferred,
                    ),
                };
            dimension_evidence.push(PricingAssumptionEvidence {
                rate_kind: range.rate_kind,
                dimension: name,
                value: value.trim().to_ascii_lowercase(),
                provenance,
                confidence,
                override_id: None,
                effective_from: None,
                effective_to: None,
                attested_at: None,
                note: None,
                source_ids: Vec::new(),
                sources: Vec::new(),
            });
        }
        deduplicate_dimension_evidence(&mut dimension_evidence);

        let mut missing_components = range
            .scenarios
            .iter()
            .flat_map(|scenario| scenario.missing_components.iter().cloned())
            .collect::<Vec<_>>();
        deduplicate_missing_components(&mut missing_components);
        let mut explanation = range
            .scenarios
            .iter()
            .flat_map(|scenario| scenario.explanation.iter().cloned())
            .collect::<Vec<_>>();
        explanation.sort();
        explanation.dedup();

        PricingMeasureEstimate {
            rate_kind: range.rate_kind,
            unit_name: range.unit_name,
            status,
            lower_bound: range.lower_bound,
            upper_bound: range.upper_bound,
            dimension_evidence,
            missing_components,
            explanation,
        }
    }

    fn pricing_evidence(
        &self,
        provider: &str,
        raw_model: &str,
        occurred_at: DateTime<Utc>,
        matched_rate_ids: &[String],
    ) -> Vec<PricingEvidence> {
        let mut evidence = Vec::new();
        if let Some(alias) = self.catalog.document.aliases.iter().find(|alias| {
            alias.provider.eq_ignore_ascii_case(provider)
                && alias.raw_model == raw_model
                && alias.interval.contains(occurred_at)
        }) {
            evidence.push(self.evidence_record(
                &alias.id,
                "model_alias",
                &alias.interval,
                &alias.source_ids,
            ));
        }
        for id in matched_rate_ids {
            if let Some(rule) = self
                .catalog
                .document
                .rates
                .iter()
                .find(|rule| rule.id == *id)
            {
                evidence.push(self.evidence_record(
                    &rule.id,
                    "rate",
                    &rule.interval,
                    &rule.source_ids,
                ));
            } else if let Some(modifier) = self
                .catalog
                .document
                .modifiers
                .iter()
                .find(|modifier| modifier.id == *id)
            {
                evidence.push(self.evidence_record(
                    &modifier.id,
                    "modifier",
                    &modifier.interval,
                    &modifier.source_ids,
                ));
            }
        }
        evidence
    }

    fn effective_auth_mode(
        &self,
        provider: &str,
        raw_model: &str,
        occurred_at: DateTime<Utc>,
        dimensions: &PricingDimensions,
    ) -> Option<String> {
        let provider = provider.trim().to_ascii_lowercase();
        let canonical_model = self
            .resolve_model(&provider, raw_model, occurred_at)
            .map(|(canonical, _)| canonical);
        canonical_model
            .as_deref()
            .and_then(|canonical| {
                self.active_dimension_override(&provider, canonical, "auth_mode", occurred_at)
            })
            .map(|override_value| override_value.value.trim().to_ascii_lowercase())
            .or_else(|| dimensions.auth_mode.clone())
    }

    fn evidence_record(
        &self,
        record_id: &str,
        record_type: &str,
        interval: &EffectiveInterval,
        source_ids: &[String],
    ) -> PricingEvidence {
        let sources = source_ids
            .iter()
            .filter_map(|id| {
                self.catalog
                    .document
                    .sources
                    .iter()
                    .find(|source| source.id == *id)
                    .cloned()
            })
            .collect();
        PricingEvidence {
            record_id: record_id.to_string(),
            record_type: record_type.to_string(),
            effective_from: interval.effective_from,
            effective_to: interval.effective_to,
            source_ids: source_ids.to_vec(),
            sources,
        }
    }

    fn estimate_kind_range(
        &self,
        provider: &str,
        raw_model: &str,
        occurred_at: DateTime<Utc>,
        usage: &UsageVector,
        dimensions: &PricingDimensions,
        kind: RateKind,
    ) -> ScenarioPriceRange {
        let provider = provider.trim().to_ascii_lowercase();
        let Some((canonical_model, _)) = self.resolve_model(&provider, raw_model, occurred_at)
        else {
            return scenario_range_from_kind(
                kind,
                BTreeMap::new(),
                KindEstimate::unpriced(
                    kind,
                    format!(
                        "No effective catalog alias for provider '{provider}', raw model '{raw_model}', at {occurred_at}."
                    ),
                ),
            );
        };
        let Some(total_input) = total_input_tokens(usage) else {
            return scenario_range_from_kind(
                kind,
                BTreeMap::new(),
                KindEstimate::unpriced(kind, "Input-token counters overflowed u64."),
            );
        };
        let candidates: Vec<&RateRule> = self
            .catalog
            .document
            .rates
            .iter()
            .filter(|rule| {
                rule.provider == provider
                    && rule.canonical_model == canonical_model
                    && rule.kind == kind
                    && rule.interval.contains(occurred_at)
                    && rule.context_matches(total_input)
            })
            .collect();
        let rule = match candidates.as_slice() {
            [rule] => *rule,
            [] => {
                return scenario_range_from_kind(
                    kind,
                    BTreeMap::new(),
                    KindEstimate::unpriced(
                        kind,
                        format!(
                            "No {kind:?} rate covers canonical model '{canonical_model}', timestamp {occurred_at}, and {total_input} input tokens."
                        ),
                    ),
                );
            }
            _ => {
                return scenario_range_from_kind(
                    kind,
                    BTreeMap::new(),
                    KindEstimate::unpriced(
                        kind,
                        format!(
                            "Multiple {kind:?} rates match canonical model '{canonical_model}'; catalog is ambiguous."
                        ),
                    ),
                );
            }
        };

        let observed = dimension_values(dimensions);
        let mut variants: Vec<(BTreeMap<String, String>, Vec<PricingAssumptionEvidence>)> =
            vec![(BTreeMap::new(), Vec::new())];
        for (name, policy) in &rule.dimension_policies {
            let choices = if let Some(override_value) =
                self.active_dimension_override(&provider, &canonical_model, name, occurred_at)
            {
                match resolve_explicit_dimension(name, policy, &override_value.value) {
                    Ok(value) => vec![(value, None)],
                    Err(reason) => {
                        return scenario_range_from_kind(
                            kind,
                            BTreeMap::new(),
                            KindEstimate::unpriced(kind, reason),
                        );
                    }
                }
            } else if let Some(value) = observed
                .get(name)
                .filter(|value| !is_explicitly_unresolved_dimension_value(value))
            {
                match resolve_explicit_dimension(name, policy, value) {
                    Ok(value) => vec![(value, None)],
                    Err(reason) => {
                        return scenario_range_from_kind(
                            kind,
                            BTreeMap::new(),
                            KindEstimate::unpriced(kind, reason),
                        );
                    }
                }
            } else {
                policy
                    .allowed
                    .iter()
                    .cloned()
                    .map(|value| {
                        let evidence = self.catalog_assumption_evidence(
                            kind,
                            name,
                            &value,
                            AssumptionProvenance::CatalogAllowedScenario,
                            &rule.interval,
                            &rule.source_ids,
                        );
                        (value, Some(evidence))
                    })
                    .collect()
            };

            if variants.len().saturating_mul(choices.len()) > 1_024 {
                return scenario_range_from_kind(
                    kind,
                    BTreeMap::new(),
                    KindEstimate::unpriced(
                        kind,
                        "Catalog scenario expansion exceeds the safety limit of 1024 combinations.",
                    ),
                );
            }
            let mut expanded = Vec::with_capacity(variants.len().saturating_mul(choices.len()));
            for (base_dimensions, base_assumptions) in variants {
                for (value, evidence) in &choices {
                    let mut scenario_dimensions = base_dimensions.clone();
                    scenario_dimensions.insert(name.clone(), value.clone());
                    let mut assumptions = base_assumptions.clone();
                    if let Some(evidence) = evidence {
                        assumptions.push(evidence.clone());
                    }
                    expanded.push((scenario_dimensions, assumptions));
                }
            }
            variants = expanded;
        }

        let scenarios = variants
            .into_iter()
            .map(|(scenario_dimensions, mut scenario_assumptions)| {
                let result = self.estimate_kind_with_forced_dimensions(
                    &provider,
                    raw_model,
                    occurred_at,
                    usage,
                    dimensions,
                    kind,
                    Some(&scenario_dimensions),
                );
                scenario_assumptions.extend(result.assumptions.clone());
                PricingScenarioEstimate {
                    dimensions: scenario_dimensions,
                    status: result.status,
                    amount: result.exact,
                    known_amount: result.known,
                    upper_amount: result.upper,
                    matched_rate_ids: result.matched_rate_ids,
                    assumptions: scenario_assumptions,
                    missing_components: result.missing,
                    explanation: result.explanation,
                }
            })
            .collect();
        build_scenario_range(kind, scenarios)
    }

    fn estimate_kind(
        &self,
        provider: &str,
        raw_model: &str,
        occurred_at: DateTime<Utc>,
        usage: &UsageVector,
        dimensions: &PricingDimensions,
        kind: RateKind,
    ) -> KindEstimate {
        self.estimate_kind_with_forced_dimensions(
            provider,
            raw_model,
            occurred_at,
            usage,
            dimensions,
            kind,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn estimate_kind_with_forced_dimensions(
        &self,
        provider: &str,
        raw_model: &str,
        occurred_at: DateTime<Utc>,
        usage: &UsageVector,
        dimensions: &PricingDimensions,
        kind: RateKind,
        forced_dimensions: Option<&BTreeMap<String, String>>,
    ) -> KindEstimate {
        let provider = provider.trim().to_ascii_lowercase();
        let Some((canonical_model, alias_id)) =
            self.resolve_model(&provider, raw_model, occurred_at)
        else {
            return KindEstimate::unpriced(
                kind,
                format!(
                    "No effective catalog alias for provider '{provider}', raw model '{raw_model}', at {occurred_at}."
                ),
            );
        };

        let Some(total_input) = total_input_tokens(usage) else {
            return KindEstimate::unpriced(kind, "Input-token counters overflowed u64.");
        };

        let candidates: Vec<&RateRule> = self
            .catalog
            .document
            .rates
            .iter()
            .filter(|rule| {
                rule.provider == provider
                    && rule.canonical_model == canonical_model
                    && rule.kind == kind
                    && rule.interval.contains(occurred_at)
                    && rule.context_matches(total_input)
            })
            .collect();

        let rule = match candidates.as_slice() {
            [rule] => *rule,
            [] => {
                return KindEstimate::unpriced(
                    kind,
                    format!(
                        "No {kind:?} rate covers canonical model '{canonical_model}', timestamp {occurred_at}, and {total_input} input tokens."
                    ),
                );
            }
            _ => {
                return KindEstimate::unpriced(
                    kind,
                    format!(
                        "Multiple {kind:?} rates match canonical model '{canonical_model}'; catalog is ambiguous."
                    ),
                );
            }
        };

        let mut dimension_values = dimension_values(dimensions);
        if let Some(forced_dimensions) = forced_dimensions {
            dimension_values.extend(forced_dimensions.clone());
        }
        let mut resolved_dimensions = BTreeMap::new();
        let mut assumptions = Vec::new();
        let mut explanation = vec![format!(
            "Resolved raw model '{raw_model}' to '{canonical_model}' with alias '{alias_id}'."
        )];
        for (name, policy) in &rule.dimension_policies {
            if let Some(override_value) =
                self.active_dimension_override(&provider, &canonical_model, name, occurred_at)
            {
                let resolved = match resolve_explicit_dimension(name, policy, &override_value.value)
                {
                    Ok(value) => value,
                    Err(reason) => return KindEstimate::unpriced(kind, reason),
                };
                let prior = dimension_values.insert(name.clone(), resolved.clone());
                explanation.push(match prior {
                    Some(prior) if prior != resolved => format!(
                        "Dimension '{name}' value '{prior}' was replaced by user-attested override '{}' value '{resolved}'.",
                        override_value.id
                    ),
                    _ => format!(
                        "Dimension '{name}' used user-attested override '{}' value '{resolved}'.",
                        override_value.id
                    ),
                });
                assumptions.push(user_override_evidence(
                    kind,
                    name,
                    &resolved,
                    override_value,
                ));
            }
            let (resolved, assumed) = match resolve_dimension(name, policy, &dimension_values) {
                Ok(value) => value,
                Err(reason) => return KindEstimate::unpriced(kind, reason),
            };
            if assumed {
                explanation.push(format!(
                    "Dimension '{name}' was absent; used documented catalog default '{resolved}'."
                ));
                assumptions.push(self.catalog_assumption_evidence(
                    kind,
                    name,
                    &resolved,
                    AssumptionProvenance::CatalogDefault,
                    &rule.interval,
                    &rule.source_ids,
                ));
            }
            resolved_dimensions.insert(name.clone(), resolved);
        }

        let mut token_multiplier = Decimal::ONE;
        let mut tool_multiplier = Decimal::ONE;
        let mut matched_rate_ids = vec![rule.id.clone()];
        for modifier in self.catalog.document.modifiers.iter().filter(|modifier| {
            modifier.provider == provider
                && modifier.canonical_model == canonical_model
                && modifier.kind == kind
                && modifier.interval.contains(occurred_at)
                && modifier
                    .selectors
                    .iter()
                    .all(|(key, expected)| resolved_dimensions.get(key) == Some(expected))
        }) {
            match modifier.scope {
                ModifierScope::TokenRates => {
                    let Some(value) =
                        checked_pricing_product(token_multiplier, modifier.multiplier.0)
                    else {
                        return KindEstimate::unpriced(
                            kind,
                            "Pricing modifier multiplication exceeds the supported decimal range.",
                        );
                    };
                    token_multiplier = value;
                }
                ModifierScope::ToolRates => {
                    let Some(value) =
                        checked_pricing_product(tool_multiplier, modifier.multiplier.0)
                    else {
                        return KindEstimate::unpriced(
                            kind,
                            "Pricing modifier multiplication exceeds the supported decimal range.",
                        );
                    };
                    tool_multiplier = value;
                }
                ModifierScope::All => {
                    let (Some(token_value), Some(tool_value)) = (
                        checked_pricing_product(token_multiplier, modifier.multiplier.0),
                        checked_pricing_product(tool_multiplier, modifier.multiplier.0),
                    ) else {
                        return KindEstimate::unpriced(
                            kind,
                            "Pricing modifier multiplication exceeds the supported decimal range.",
                        );
                    };
                    token_multiplier = token_value;
                    tool_multiplier = tool_value;
                }
            }
            matched_rate_ids.push(modifier.id.clone());
            explanation.push(format!(
                "Applied modifier '{}' ({}x, {:?}).",
                modifier.id, modifier.multiplier.0, modifier.scope
            ));
        }

        // Codex credits publish input, cached-input, and output rates only.
        // Cache-write tokens are a subset of non-cached input for this measure,
        // not a fourth credit class. API-equivalent USD keeps cache writes
        // separate because the API card publishes their 1.25x rate.
        let input_quantity = if kind == RateKind::CodexCredits {
            usage
                .input_tokens_total
                .saturating_sub(usage.input_tokens_cached)
        } else {
            usage.input_tokens_uncached
        };
        let include_cache_write_components = kind == RateKind::UsdApiEquivalent;
        let token_components = [
            ("input_uncached", input_quantity, rule.rates.input),
            (
                "cache_read",
                usage.input_tokens_cached,
                rule.rates.cache_read,
            ),
            (
                "cache_write_unknown",
                if include_cache_write_components {
                    usage.cache_write_unknown_tokens
                } else {
                    0
                },
                rule.rates.cache_write,
            ),
            (
                "cache_write_5m",
                if include_cache_write_components {
                    usage.cache_write_5m_tokens
                } else {
                    0
                },
                rule.rates.cache_write_5m,
            ),
            (
                "cache_write_1h",
                if include_cache_write_components {
                    usage.cache_write_1h_tokens
                } else {
                    0
                },
                rule.rates.cache_write_1h,
            ),
            ("output", usage.output_tokens_total, rule.rates.output),
        ];

        let mut subtotal = Decimal::ZERO;
        let mut priced_component_count = 0usize;
        let mut missing = Vec::new();
        for (component, quantity, rate) in token_components {
            if quantity == 0 {
                continue;
            }
            let Some(rate) = rate else {
                missing.push(MissingPriceComponent {
                    rate_kind: kind,
                    component: component.to_string(),
                    quantity: Some(quantity),
                    reason: format!(
                        "Rate '{}' does not publish a price for this token class.",
                        rule.id
                    ),
                });
                continue;
            };
            let Some(effective_rate) = checked_pricing_product(rate.0, token_multiplier) else {
                return KindEstimate::unpriced(
                    kind,
                    "Effective token rate exceeds the supported decimal range.",
                );
            };
            let Some(component_cost) =
                checked_component_cost(quantity, effective_rate, rule.unit_scale)
            else {
                return KindEstimate::unpriced(
                    kind,
                    "Token pricing product exceeds the supported decimal range.",
                );
            };
            let Some(next_subtotal) = checked_pricing_sum(subtotal, component_cost) else {
                return KindEstimate::unpriced(
                    kind,
                    "Pricing subtotal exceeds the supported decimal range.",
                );
            };
            subtotal = next_subtotal;
            priced_component_count += 1;
            explanation.push(format!(
                "{component}: {quantity} × {effective_rate} {} / {} = {component_cost} {}.",
                kind.unit_name(),
                rule.unit_scale,
                kind.unit_name()
            ));
        }

        let tool_components = [
            ("web_search_requests", usage.web_search_requests),
            ("web_fetch_requests", usage.web_fetch_requests),
        ];
        for (component, quantity) in tool_components {
            if quantity == 0 {
                continue;
            }
            let Some(rate) = rule.tool_rates.get(component) else {
                missing.push(MissingPriceComponent {
                    rate_kind: kind,
                    component: component.to_string(),
                    quantity: Some(quantity),
                    reason: format!("Rate '{}' has no verified per-tool-unit price.", rule.id),
                });
                continue;
            };
            let Some(effective_rate) = checked_pricing_product(rate.rate.0, tool_multiplier) else {
                return KindEstimate::unpriced(
                    kind,
                    "Effective tool rate exceeds the supported decimal range.",
                );
            };
            let Some(component_cost) =
                checked_component_cost(quantity, effective_rate, rate.per_units)
            else {
                return KindEstimate::unpriced(
                    kind,
                    "Tool pricing product exceeds the supported decimal range.",
                );
            };
            let Some(next_subtotal) = checked_pricing_sum(subtotal, component_cost) else {
                return KindEstimate::unpriced(
                    kind,
                    "Pricing subtotal exceeds the supported decimal range.",
                );
            };
            subtotal = next_subtotal;
            priced_component_count += 1;
            explanation.push(format!(
                "{component}: {quantity} × {effective_rate} {} / {} requests = {component_cost} {}{}.",
                kind.unit_name(),
                rate.per_units,
                kind.unit_name(),
                if rate.explicit_free { " (explicitly free)" } else { "" }
            ));
        }

        if kind == RateKind::UsdApiEquivalent
            && rule.cache_write_observation_required
            && dimensions.cache_write_data_complete != Some(true)
        {
            missing.push(MissingPriceComponent {
                rate_kind: kind,
                component: "cache_write_observation".to_string(),
                quantity: None,
                reason: "The event does not establish that its billable cache-write counters are complete."
                    .to_string(),
            });
        }

        if missing.is_empty() {
            KindEstimate {
                status: EstimateStatus::Priced,
                exact: Some(subtotal),
                known: Some(subtotal),
                upper: Some(subtotal),
                matched_rate_ids,
                assumptions,
                missing,
                explanation,
            }
        } else {
            let bounded_cache_write = (provider == "openai"
                && kind == RateKind::UsdApiEquivalent
                && dimensions.input_subset_accounting_consistent != Some(false)
                && missing.len() == 1
                && missing[0].component == "cache_write_observation")
                .then(|| {
                    let input_rate =
                        checked_pricing_product(rule.rates.input?.0, token_multiplier)?;
                    let write_rate =
                        checked_pricing_product(rule.rates.cache_write?.0, token_multiplier)?;
                    let input_cost = checked_component_cost(
                        usage.input_tokens_uncached,
                        input_rate,
                        rule.unit_scale,
                    )?;
                    let reclassified_cost = checked_component_cost(
                        usage.input_tokens_uncached,
                        write_rate,
                        rule.unit_scale,
                    )?;
                    let other = checked_pricing_difference(subtotal, input_cost)?;
                    Some((
                        checked_pricing_sum(other, input_cost.min(reclassified_cost))?,
                        checked_pricing_sum(other, input_cost.max(reclassified_cost))?,
                    ))
                })
                .flatten();
            explanation.push(format!(
                "This is partial: {} billable component(s) could not be priced.",
                missing.len()
            ));
            if let Some((lower, upper)) = bounded_cache_write {
                explanation.push(format!(
                    "Unreported OpenAI cache writes are bounded by the inclusive input counter: reclassifying between 0 and {} non-cached input tokens gives {lower} to {upper} {}.",
                    usage.input_tokens_uncached,
                    kind.unit_name()
                ));
            }
            KindEstimate {
                status: EstimateStatus::Partial,
                exact: None,
                // A partial estimate never exposes a numeric zero merely
                // because its only known component is explicitly free.
                known: bounded_cache_write.map(|(lower, _)| lower).or_else(|| {
                    (priced_component_count > 0 && subtotal != Decimal::ZERO).then_some(subtotal)
                }),
                upper: bounded_cache_write.map(|(_, upper)| upper),
                matched_rate_ids,
                assumptions,
                missing,
                explanation,
            }
        }
    }

    fn resolve_model(
        &self,
        provider: &str,
        raw_model: &str,
        occurred_at: DateTime<Utc>,
    ) -> Option<(String, String)> {
        let matches: Vec<&ModelAlias> = self
            .catalog
            .document
            .aliases
            .iter()
            .filter(|alias| {
                alias.provider == provider
                    && alias.raw_model == raw_model
                    && alias.interval.contains(occurred_at)
            })
            .collect();
        match matches.as_slice() {
            [alias] => Some((alias.canonical_model.clone(), alias.id.clone())),
            _ => None,
        }
    }

    /// Parse and structurally verify a candidate without mutating catalog
    /// state. A current, downgrade, or revision-conflicting candidate is
    /// reported explicitly and is never eligible for `update`.
    pub fn check_candidate(
        &self,
        bytes: &[u8],
        expected_sha256: Option<&str>,
    ) -> Result<CatalogCheckResult, PricingError> {
        if let Some(expected_sha256) = expected_sha256 {
            verify_sha256(bytes, expected_sha256)?;
        }
        let candidate = PriceCatalog::parse(bytes)?;
        require_verified(&candidate)?;
        Ok(self.check_catalog(&candidate))
    }

    pub fn check_catalog(&self, candidate: &PriceCatalog) -> CatalogCheckResult {
        let relation = if candidate.sha256() == self.catalog.sha256() {
            CatalogCandidateRelation::Current
        } else if candidate.revision() == self.catalog.revision() {
            CatalogCandidateRelation::RevisionConflict
        } else if candidate.published_at() <= self.catalog.published_at() {
            CatalogCandidateRelation::Downgrade
        } else {
            CatalogCandidateRelation::Newer
        };
        CatalogCheckResult {
            active_revision: self.catalog.revision().to_string(),
            active_sha256: self.catalog.sha256().to_string(),
            active_published_at: self.catalog.published_at(),
            candidate_revision: candidate.revision().to_string(),
            candidate_sha256: candidate.sha256().to_string(),
            candidate_published_at: candidate.published_at(),
            relation,
            update_allowed: relation == CatalogCandidateRelation::Newer,
        }
    }

    /// Parse and structurally verify a candidate, then reject revisions that
    /// are not newer than the active catalog. Transport authenticity remains a
    /// caller concern; use the checksum variant when a trusted source supplies
    /// a digest.
    pub fn validate_candidate(&self, bytes: &[u8]) -> Result<PriceCatalog, PricingError> {
        let candidate = PriceCatalog::parse(bytes)?;
        require_verified(&candidate)?;
        require_update_allowed(&self.check_catalog(&candidate))?;
        Ok(candidate)
    }

    pub fn validate_candidate_with_checksum(
        &self,
        bytes: &[u8],
        expected_sha256: &str,
    ) -> Result<PriceCatalog, PricingError> {
        verify_sha256(bytes, expected_sha256)?;
        self.validate_candidate(bytes)
    }

    /// Load one exact verified revision from the active file, immutable
    /// history, or the bundled catalog. Reused revision labels with different
    /// bytes are rejected as ambiguous rather than selected arbitrarily.
    pub fn load_revision(path: &Path, revision: &str) -> Result<Self, PricingError> {
        let catalog = catalog_by_revision(path, revision)?;
        Ok(Self {
            catalog,
            dimension_overrides: Vec::new(),
        })
    }

    pub fn diff_revisions(
        path: &Path,
        from_revision: &str,
        to_revision: &str,
    ) -> Result<CatalogDiff, PricingError> {
        let from = catalog_by_revision(path, from_revision)?;
        let to = catalog_by_revision(path, to_revision)?;
        diff_catalogs(&from, &to)
    }

    /// Validate and durably install a candidate in one operation. Passing a
    /// checksum keeps the same trusted-digest check used by
    /// [`Self::validate_candidate_with_checksum`]. The active engine catalog
    /// is retained as rollback history even when `path` does not exist yet.
    pub fn install_candidate(
        &self,
        bytes: &[u8],
        expected_sha256: Option<&str>,
        path: &Path,
    ) -> Result<CatalogInstallReceipt, PricingError> {
        let candidate = match expected_sha256 {
            Some(expected) => self.validate_candidate_with_checksum(bytes, expected)?,
            None => self.validate_candidate(bytes)?,
        };
        self.install_validated_candidate(&candidate, path)
    }

    /// Durably install a candidate previously returned by one of the
    /// validation methods. This is useful to callers that need candidate
    /// metadata before committing the update.
    pub fn install_validated_candidate(
        &self,
        catalog: &PriceCatalog,
        path: &Path,
    ) -> Result<CatalogInstallReceipt, PricingError> {
        install_validated_catalog(catalog, path, Some(&self.catalog))
    }

    /// Explicitly activate a verified historical revision. Unlike an update,
    /// this operation intentionally permits moving backward. Both old and new
    /// bytes remain in immutable history and the active replacement is atomic.
    pub fn activate_revision(
        &self,
        revision: &str,
        path: &Path,
    ) -> Result<CatalogInstallReceipt, PricingError> {
        let candidate = catalog_by_revision(path, revision)?;
        if candidate.sha256() == self.catalog.sha256() {
            return Err(PricingError::UpdateRejected(format!(
                "revision '{}' is already active",
                candidate.revision()
            )));
        }
        install_catalog(
            candidate,
            path,
            Some(&self.catalog),
            InstallPolicy::Historical,
        )
    }

    /// Select the newest verified retained revision strictly older than the
    /// active `(published_at, revision)` key and activate it atomically.
    pub fn rollback(&self, path: &Path) -> Result<CatalogInstallReceipt, PricingError> {
        let active_key = (
            self.catalog.published_at(),
            self.catalog.revision().to_string(),
        );
        let retained_sha256 = Self::retained_revisions(path)?
            .into_iter()
            .map(|revision| revision.sha256)
            .collect::<HashSet<_>>();
        let mut candidates = catalog_candidates(path)?;
        candidates.retain(|candidate| {
            retained_sha256.contains(candidate.sha256())
                && candidate.sha256() != self.catalog.sha256()
                && (candidate.published_at(), candidate.revision().to_string()) < active_key
        });
        candidates.sort_by(|left, right| {
            left.published_at()
                .cmp(&right.published_at())
                .then_with(|| left.revision().cmp(right.revision()))
        });
        let candidate = candidates.pop().ok_or_else(|| {
            PricingError::UpdateRejected(format!(
                "no verified retained revision is older than active revision '{}'",
                self.catalog.revision()
            ))
        })?;
        install_catalog(
            candidate,
            path,
            Some(&self.catalog),
            InstallPolicy::Historical,
        )
    }

    /// Return the sibling directory used for immutable revision history.
    /// For `prices.json`, this is `prices.history`.
    pub fn history_dir(path: &Path) -> PathBuf {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("catalog");
        parent.join(format!("{stem}.history"))
    }

    /// Enumerate and verify retained revision files. A malformed history file
    /// is reported instead of being omitted, keeping rollback state auditable.
    pub fn retained_revisions(path: &Path) -> Result<Vec<RetainedCatalogRevision>, PricingError> {
        let history_dir = Self::history_dir(path);
        let entries = match fs::read_dir(&history_dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(PricingError::Read {
                    path: history_dir.display().to_string(),
                    source,
                });
            }
        };
        let mut retained = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| PricingError::Read {
                path: history_dir.display().to_string(),
                source,
            })?;
            if !entry
                .file_type()
                .map_err(|source| PricingError::Read {
                    path: entry.path().display().to_string(),
                    source,
                })?
                .is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|source| PricingError::Read {
                path: entry.path().display().to_string(),
                source,
            })?;
            let catalog = PriceCatalog::parse(&bytes)?;
            require_verified(&catalog)?;
            let expected_path = revision_snapshot_path(path, &catalog);
            if entry.path().file_name() != expected_path.file_name() {
                return Err(PricingError::Verification(format!(
                    "retained catalog {} is not named for revision '{}' and checksum {}",
                    entry.path().display(),
                    catalog.revision(),
                    catalog.sha256()
                )));
            }
            retained.push(retained_revision(&catalog, entry.path()));
        }
        retained.sort_by(|left, right| {
            left.revision
                .cmp(&right.revision)
                .then_with(|| left.sha256.cmp(&right.sha256))
        });
        Ok(retained)
    }

    /// Atomically save verified catalog bytes and retain any file currently at
    /// `path`. This compatibility entry point cannot retain an in-memory
    /// bundled catalog when `path` is absent; prefer [`Self::install_candidate`]
    /// or [`Self::install_validated_candidate`] for updates from an engine.
    pub fn save_candidate(catalog: &PriceCatalog, path: &Path) -> Result<(), PricingError> {
        install_validated_catalog(catalog, path, None).map(|_| ())
    }
}

fn install_validated_catalog(
    candidate: &PriceCatalog,
    path: &Path,
    fallback_previous: Option<&PriceCatalog>,
) -> Result<CatalogInstallReceipt, PricingError> {
    install_catalog(
        candidate.clone(),
        path,
        fallback_previous,
        InstallPolicy::Update,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallPolicy {
    Update,
    Historical,
}

fn install_catalog(
    candidate: PriceCatalog,
    path: &Path,
    fallback_previous: Option<&PriceCatalog>,
    policy: InstallPolicy,
) -> Result<CatalogInstallReceipt, PricingError> {
    // PriceCatalog::parse is public for inspection, so installation repeats
    // structural verification rather than trusting the caller's provenance.
    require_verified(&candidate)?;

    let parent = nonempty_parent(path);
    fs::create_dir_all(parent).map_err(|source| PricingError::Write {
        path: parent.display().to_string(),
        source,
    })?;

    let active_catalog = match fs::read(path) {
        Ok(bytes) => {
            let catalog = PriceCatalog::parse(&bytes)?;
            require_verified(&catalog)?;
            Some(catalog)
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(PricingError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let previous = active_catalog.as_ref().or(fallback_previous);
    if let Some(previous) = previous {
        if policy == InstallPolicy::Update {
            require_newer_catalog(&candidate, previous)?;
        } else if previous.sha256() == candidate.sha256() {
            return Err(PricingError::UpdateRejected(format!(
                "revision '{}' is already active",
                candidate.revision()
            )));
        }
    }

    // Both snapshots are published before the active-file commit. Therefore a
    // history failure cannot leave a new active catalog without rollback data.
    let mut retained = Vec::new();
    if let Some(previous) = previous
        && previous.sha256() != candidate.sha256()
    {
        retained.push(retain_catalog_snapshot(path, previous)?);
    }
    retained.push(retain_catalog_snapshot(path, &candidate)?);
    retained.sort_by(|left, right| {
        left.revision
            .cmp(&right.revision)
            .then_with(|| left.sha256.cmp(&right.sha256))
    });
    retained.dedup_by(|left, right| left.sha256 == right.sha256);

    let staged_path = write_durable_temp(path, candidate.raw_bytes())?;
    let staged_result = (|| {
        let staged_bytes = fs::read(&staged_path).map_err(|source| PricingError::Read {
            path: staged_path.display().to_string(),
            source,
        })?;
        let calculated = sha256_hex(&staged_bytes);
        if calculated != candidate.sha256() {
            return Err(PricingError::ChecksumMismatch {
                expected: candidate.sha256().to_string(),
                calculated,
            });
        }
        let staged_catalog = PriceCatalog::parse(&staged_bytes)?;
        require_verified(&staged_catalog)?;
        atomic_replace(&staged_path, path).map_err(|source| PricingError::Write {
            path: path.display().to_string(),
            source,
        })?;
        Ok(())
    })();
    if staged_result.is_err() {
        let _ = fs::remove_file(&staged_path);
    }
    staged_result?;

    Ok(CatalogInstallReceipt {
        active_path: path.to_path_buf(),
        installed_revision: candidate.revision().to_string(),
        installed_sha256: candidate.sha256().to_string(),
        history_dir: PricingEngine::history_dir(path),
        retained_revisions: retained,
    })
}

fn require_newer_catalog(
    candidate: &PriceCatalog,
    previous: &PriceCatalog,
) -> Result<(), PricingError> {
    if candidate.sha256() == previous.sha256() || candidate.revision() == previous.revision() {
        return Err(PricingError::UpdateRejected(format!(
            "revision '{}' is already installed",
            candidate.revision()
        )));
    }
    if candidate.published_at() <= previous.published_at() {
        return Err(PricingError::UpdateRejected(format!(
            "candidate publication time {} is not newer than {}",
            candidate.published_at(),
            previous.published_at()
        )));
    }
    Ok(())
}

fn require_update_allowed(check: &CatalogCheckResult) -> Result<(), PricingError> {
    match check.relation {
        CatalogCandidateRelation::Newer => Ok(()),
        CatalogCandidateRelation::Current => Err(PricingError::UpdateRejected(format!(
            "revision '{}' is already installed",
            check.candidate_revision
        ))),
        CatalogCandidateRelation::Downgrade => Err(PricingError::UpdateRejected(format!(
            "candidate revision '{}' published at {} is not newer than active revision '{}' published at {}; use `prices activate {}` for an intentional historical selection",
            check.candidate_revision,
            check.candidate_published_at,
            check.active_revision,
            check.active_published_at,
            check.candidate_revision
        ))),
        CatalogCandidateRelation::RevisionConflict => Err(PricingError::UpdateRejected(format!(
            "candidate reuses active revision '{}' with different bytes",
            check.candidate_revision
        ))),
    }
}

fn catalog_candidates(path: &Path) -> Result<Vec<PriceCatalog>, PricingError> {
    let mut by_sha = BTreeMap::<String, PriceCatalog>::new();
    let bundled = PriceCatalog::parse(BUNDLED_CATALOG)?;
    require_verified(&bundled)?;
    by_sha.insert(bundled.sha256().to_string(), bundled);

    match fs::read(path) {
        Ok(bytes) => {
            let active = PriceCatalog::parse(&bytes)?;
            require_verified(&active)?;
            by_sha.insert(active.sha256().to_string(), active);
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(PricingError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    }

    for retained in PricingEngine::retained_revisions(path)? {
        let bytes = fs::read(&retained.path).map_err(|source| PricingError::Read {
            path: retained.path.display().to_string(),
            source,
        })?;
        let catalog = PriceCatalog::parse(&bytes)?;
        require_verified(&catalog)?;
        if catalog.sha256() != retained.sha256 {
            return Err(PricingError::Verification(format!(
                "retained catalog {} changed after enumeration",
                retained.path.display()
            )));
        }
        by_sha.insert(catalog.sha256().to_string(), catalog);
    }
    Ok(by_sha.into_values().collect())
}

fn catalog_by_revision(path: &Path, revision: &str) -> Result<PriceCatalog, PricingError> {
    let revision = revision.trim();
    if revision.is_empty() {
        return Err(PricingError::Verification(
            "catalog revision selector cannot be empty".to_string(),
        ));
    }
    let mut matches = catalog_candidates(path)?
        .into_iter()
        .filter(|catalog| catalog.revision() == revision)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(PricingError::Read {
            path: PricingEngine::history_dir(path).display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("verified catalog revision '{revision}' was not found"),
            ),
        });
    }
    matches.sort_by(|left, right| left.sha256().cmp(right.sha256()));
    matches.dedup_by(|left, right| left.sha256() == right.sha256());
    if matches.len() != 1 {
        return Err(PricingError::Verification(format!(
            "catalog revision '{revision}' is ambiguous because immutable history contains {} distinct checksums",
            matches.len()
        )));
    }
    Ok(matches.remove(0))
}

fn diff_catalogs(from: &PriceCatalog, to: &PriceCatalog) -> Result<CatalogDiff, PricingError> {
    let from_value: serde_json::Value = serde_json::from_slice(from.raw_bytes())?;
    let to_value: serde_json::Value = serde_json::from_slice(to.raw_bytes())?;
    let mut metadata_changed = Vec::new();
    for field in [
        "schema_version",
        "published_at",
        "verified_at",
        "stale_after_days",
        "coverage_note",
    ] {
        if from_value.get(field) != to_value.get(field) {
            metadata_changed.push(field.to_string());
        }
    }
    Ok(CatalogDiff {
        from_revision: from.revision().to_string(),
        from_sha256: from.sha256().to_string(),
        to_revision: to.revision().to_string(),
        to_sha256: to.sha256().to_string(),
        metadata_changed,
        sources: collection_diff(&from_value, &to_value, "sources")?,
        aliases: collection_diff(&from_value, &to_value, "aliases")?,
        rates: collection_diff(&from_value, &to_value, "rates")?,
        modifiers: collection_diff(&from_value, &to_value, "modifiers")?,
    })
}

fn collection_diff(
    from: &serde_json::Value,
    to: &serde_json::Value,
    field: &str,
) -> Result<CatalogCollectionDiff, PricingError> {
    fn records(
        document: &serde_json::Value,
        field: &str,
    ) -> Result<BTreeMap<String, serde_json::Value>, PricingError> {
        let mut records = BTreeMap::new();
        let values = match document.get(field) {
            Some(value) => value.as_array().ok_or_else(|| {
                PricingError::Verification(format!("catalog field '{field}' is not an array"))
            })?,
            None => return Ok(records),
        };
        for value in values {
            let id = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    PricingError::Verification(format!(
                        "catalog {field} record is missing a string id"
                    ))
                })?;
            if records.insert(id.to_string(), value.clone()).is_some() {
                return Err(PricingError::Verification(format!(
                    "catalog {field} contains duplicate id '{id}'"
                )));
            }
        }
        Ok(records)
    }

    let from = records(from, field)?;
    let to = records(to, field)?;
    let added = to
        .keys()
        .filter(|id| !from.contains_key(*id))
        .cloned()
        .collect();
    let removed = from
        .keys()
        .filter(|id| !to.contains_key(*id))
        .cloned()
        .collect();
    let changed = from
        .iter()
        .filter_map(|(id, value)| {
            to.get(id)
                .filter(|candidate| *candidate != value)
                .map(|_| id.clone())
        })
        .collect();
    Ok(CatalogCollectionDiff {
        added,
        removed,
        changed,
    })
}

fn retain_catalog_snapshot(
    active_path: &Path,
    catalog: &PriceCatalog,
) -> Result<RetainedCatalogRevision, PricingError> {
    let history_dir = PricingEngine::history_dir(active_path);
    fs::create_dir_all(&history_dir).map_err(|source| PricingError::Write {
        path: history_dir.display().to_string(),
        source,
    })?;
    let snapshot_path = revision_snapshot_path(active_path, catalog);

    match fs::read(&snapshot_path) {
        Ok(existing) if existing == catalog.raw_bytes() => {
            return Ok(retained_revision(catalog, snapshot_path));
        }
        Ok(_) => {
            return Err(PricingError::Verification(format!(
                "retained catalog {} does not match its revision/checksum name",
                snapshot_path.display()
            )));
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(PricingError::Read {
                path: snapshot_path.display().to_string(),
                source,
            });
        }
    }

    let staged_path = write_durable_temp(&snapshot_path, catalog.raw_bytes())?;
    let publish_result =
        publish_new_history_file(&staged_path, &snapshot_path, catalog.raw_bytes());
    if publish_result.is_err() {
        let _ = fs::remove_file(&staged_path);
    }
    publish_result?;
    sync_directory(&history_dir).map_err(|source| PricingError::Write {
        path: history_dir.display().to_string(),
        source,
    })?;
    Ok(retained_revision(catalog, snapshot_path))
}

fn retained_revision(catalog: &PriceCatalog, path: PathBuf) -> RetainedCatalogRevision {
    RetainedCatalogRevision {
        revision: catalog.revision().to_string(),
        sha256: catalog.sha256().to_string(),
        path,
    }
}

fn revision_snapshot_path(active_path: &Path, catalog: &PriceCatalog) -> PathBuf {
    PricingEngine::history_dir(active_path).join(format!(
        "{}--{}.json",
        sanitized_revision(catalog.revision()),
        catalog.sha256()
    ))
}

fn sanitized_revision(revision: &str) -> String {
    let mut sanitized = revision
        .chars()
        .take(80)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        sanitized.push_str("revision");
    }
    sanitized
}

fn nonempty_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn write_durable_temp(target: &Path, bytes: &[u8]) -> Result<PathBuf, PricingError> {
    let parent = nonempty_parent(target);
    let target_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("catalog");
    for _ in 0..128 {
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{target_name}.tmp-{}-{sequence}",
            std::process::id()
        ));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(PricingError::Write {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let write_result = file
            .write_all(bytes)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all());
        if let Err(source) = write_result {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(PricingError::Write {
                path: path.display().to_string(),
                source,
            });
        }
        return Ok(path);
    }
    Err(PricingError::Write {
        path: target.display().to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique catalog staging file",
        ),
    })
}

fn publish_new_history_file(
    staged_path: &Path,
    snapshot_path: &Path,
    expected_bytes: &[u8],
) -> Result<(), PricingError> {
    match fs::hard_link(staged_path, snapshot_path) {
        Ok(()) => fs::remove_file(staged_path).map_err(|source| PricingError::Write {
            path: staged_path.display().to_string(),
            source,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(snapshot_path).map_err(|source| PricingError::Read {
                path: snapshot_path.display().to_string(),
                source,
            })?;
            if existing == expected_bytes {
                fs::remove_file(staged_path).map_err(|source| PricingError::Write {
                    path: staged_path.display().to_string(),
                    source,
                })
            } else {
                Err(PricingError::Verification(format!(
                    "retained catalog {} changed during installation",
                    snapshot_path.display()
                )))
            }
        }
        Err(source) => Err(PricingError::Write {
            path: snapshot_path.display().to_string(),
            source,
        }),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    // Windows has no stable std API for opening a directory handle. Each
    // snapshot file is fully synced before its atomic hard-link publication.
    Ok(())
}

fn atomic_replace(staged_path: &Path, active_path: &Path) -> std::io::Result<()> {
    // std::fs::rename is a replacement operation when the destination file
    // already exists. Rust maps it to rename(2) on Unix and to MoveFileExW or
    // SetFileInformationByHandle on Windows. Keeping both paths in one
    // directory prevents a cross-filesystem copy/delete fallback.
    fs::rename(staged_path, active_path)
}

fn total_input_tokens(usage: &UsageVector) -> Option<u64> {
    if usage.input_tokens_total > 0 {
        Some(usage.input_tokens_total)
    } else {
        usage
            .input_tokens_uncached
            .checked_add(usage.input_tokens_cached)
            .and_then(|value| value.checked_add(usage.cache_write_5m_tokens))
            .and_then(|value| value.checked_add(usage.cache_write_1h_tokens))
            .and_then(|value| value.checked_add(usage.cache_write_unknown_tokens))
    }
}

fn validate_dimension_overrides(
    dimension_overrides: &[PricingDimensionOverride],
) -> Result<(), PricingError> {
    let mut ids = BTreeSet::new();
    for override_value in dimension_overrides {
        override_value
            .validate()
            .map_err(|error| PricingError::InvalidOverride(error.to_string()))?;
        if !ids.insert(override_value.id.clone()) {
            return Err(PricingError::InvalidOverride(format!(
                "override id '{}' is duplicated",
                override_value.id
            )));
        }
    }
    for (index, left) in dimension_overrides.iter().enumerate() {
        for right in dimension_overrides.iter().skip(index + 1) {
            let same_target = left.provider.eq_ignore_ascii_case(&right.provider)
                && left.canonical_model == right.canonical_model
                && left.dimension.eq_ignore_ascii_case(&right.dimension);
            let overlaps = left.effective_from < right.effective_to
                && right.effective_from < left.effective_to;
            if same_target && overlaps {
                return Err(PricingError::InvalidOverride(format!(
                    "overrides '{}' and '{}' overlap for the same target",
                    left.id, right.id
                )));
            }
        }
    }
    Ok(())
}

fn user_override_evidence(
    rate_kind: RateKind,
    dimension: &str,
    value: &str,
    override_value: &PricingDimensionOverride,
) -> PricingAssumptionEvidence {
    PricingAssumptionEvidence {
        rate_kind,
        dimension: dimension.to_string(),
        value: value.to_string(),
        provenance: AssumptionProvenance::UserAttestedOverride,
        confidence: PricingDimensionConfidence::Attested,
        override_id: Some(override_value.id.clone()),
        effective_from: Some(override_value.effective_from),
        effective_to: Some(override_value.effective_to),
        attested_at: Some(override_value.attested_at),
        note: override_value.note.clone(),
        source_ids: Vec::new(),
        sources: Vec::new(),
    }
}

fn scenario_range_from_kind(
    kind: RateKind,
    dimensions: BTreeMap<String, String>,
    result: KindEstimate,
) -> ScenarioPriceRange {
    build_scenario_range(
        kind,
        vec![PricingScenarioEstimate {
            dimensions,
            status: result.status,
            amount: result.exact,
            known_amount: result.known,
            upper_amount: result.upper,
            matched_rate_ids: result.matched_rate_ids,
            assumptions: result.assumptions,
            missing_components: result.missing,
            explanation: result.explanation,
        }],
    )
}

fn build_scenario_range(
    kind: RateKind,
    scenarios: Vec<PricingScenarioEstimate>,
) -> ScenarioPriceRange {
    let all_exact = !scenarios.is_empty()
        && scenarios
            .iter()
            .all(|scenario| scenario.status == EstimateStatus::Priced && scenario.amount.is_some());
    let all_unpriced = scenarios
        .iter()
        .all(|scenario| scenario.status == EstimateStatus::Unpriced);

    let finite_bounds: Option<Vec<(Decimal, Decimal)>> = scenarios
        .iter()
        .map(|scenario| {
            let lower = scenario.amount.or(scenario.known_amount)?;
            let upper = scenario.amount.or(scenario.upper_amount)?;
            Some((lower, upper))
        })
        .collect();

    let (status, lower_bound, upper_bound) = if all_exact {
        let mut amounts = scenarios.iter().filter_map(|scenario| scenario.amount);
        let first = amounts.next().expect("all_exact guarantees an amount");
        let (lower, upper) = amounts.fold((first, first), |(lower, upper), amount| {
            (lower.min(amount), upper.max(amount))
        });
        let status = if lower == upper {
            PriceRangeStatus::Exact
        } else {
            PriceRangeStatus::Bounded
        };
        (status, Some(lower), Some(upper))
    } else if all_unpriced {
        (PriceRangeStatus::Unpriced, None, None)
    } else if let Some(bounds) = finite_bounds {
        let lower = bounds
            .iter()
            .map(|(lower, _)| *lower)
            .min()
            .expect("finite bounds require at least one scenario");
        let upper = bounds
            .iter()
            .map(|(_, upper)| *upper)
            .max()
            .expect("finite bounds require at least one scenario");
        let status = if lower == upper {
            PriceRangeStatus::Exact
        } else {
            PriceRangeStatus::Bounded
        };
        (status, Some(lower), Some(upper))
    } else {
        let known: Option<Vec<Decimal>> = scenarios
            .iter()
            .map(|scenario| scenario.amount.or(scenario.known_amount))
            .collect();
        let lower = known.and_then(|values| values.into_iter().min());
        (PriceRangeStatus::Partial, lower, None)
    };

    ScenarioPriceRange {
        rate_kind: kind,
        unit_name: kind.unit_name().to_string(),
        status,
        lower_bound,
        upper_bound,
        scenarios,
    }
}

fn dimension_value_and_provenance<'a>(
    dimensions: &'a PricingDimensions,
    name: &str,
) -> Option<(&'a str, Option<DimensionValueProvenance>)> {
    match name {
        "auth_mode" => dimensions
            .auth_mode
            .as_deref()
            .map(|value| (value, dimensions.auth_mode_provenance)),
        "provider_route" => dimensions
            .provider_route
            .as_deref()
            .map(|value| (value, dimensions.provider_route_provenance)),
        "service_tier" => dimensions
            .service_tier
            .as_deref()
            .map(|value| (value, dimensions.service_tier_provenance)),
        "speed" => dimensions
            .speed
            .as_deref()
            .map(|value| (value, dimensions.speed_provenance)),
        "inference_geo" => dimensions
            .inference_geo
            .as_deref()
            .map(|value| (value, dimensions.inference_geo_provenance)),
        _ => None,
    }
}

fn dimensions_with_legacy_provenance(
    dimensions: &PricingDimensions,
    warnings: &[String],
) -> PricingDimensions {
    let mut result = dimensions.clone();
    if result.auth_mode.is_some()
        && result.auth_mode_provenance.is_none()
        && warnings.iter().any(|warning| {
            warning.contains("auth mode was inferred from the current Codex profile")
        })
    {
        result.auth_mode_provenance = Some(DimensionValueProvenance::CurrentProfileInferred);
    }
    if result.input_subset_accounting_consistent.is_none()
        && warnings
            .iter()
            .any(|warning| warning == "input_subsets_exceed_total_input")
    {
        result.input_subset_accounting_consistent = Some(false);
    }
    result
}

fn deduplicate_dimension_evidence(values: &mut Vec<PricingAssumptionEvidence>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| {
        seen.insert(format!(
            "{:?}|{}|{}|{:?}|{}",
            value.rate_kind,
            value.dimension,
            value.value,
            value.provenance,
            value.override_id.as_deref().unwrap_or_default()
        ))
    });
}

fn deduplicate_missing_components(values: &mut Vec<MissingPriceComponent>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| {
        seen.insert(format!(
            "{:?}|{}|{:?}|{}",
            value.rate_kind, value.component, value.quantity, value.reason
        ))
    });
}

fn dimension_values(dimensions: &PricingDimensions) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for (key, value) in [
        ("auth_mode", dimensions.auth_mode.as_ref()),
        ("provider_route", dimensions.provider_route.as_ref()),
        ("service_tier", dimensions.service_tier.as_ref()),
        ("speed", dimensions.speed.as_ref()),
        ("inference_geo", dimensions.inference_geo.as_ref()),
    ] {
        if let Some(value) = value {
            values.insert(key.to_string(), value.trim().to_ascii_lowercase());
        }
    }
    // Codex records Fast mode as service_tier="fast" in some client
    // versions, while newer envelopes may expose a dedicated speed field.
    if !values.contains_key("speed")
        && values
            .get("service_tier")
            .is_some_and(|value| value == "fast")
    {
        values.insert("speed".to_string(), "fast".to_string());
    }
    values
}

fn is_api_key_auth_mode(value: &str) -> bool {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
        .contains("apikey")
}

fn resolve_dimension(
    name: &str,
    policy: &DimensionPolicy,
    values: &BTreeMap<String, String>,
) -> Result<(String, bool), String> {
    let (raw, assumed) = match values.get(name) {
        Some(value) => {
            if is_explicitly_unresolved_dimension_value(value) {
                return Err(format!(
                    "Pricing dimension '{name}' is explicitly unresolved ('{value}'); a documented scenario or bounded user attestation is required."
                ));
            }
            (value.clone(), false)
        }
        None => match &policy.default {
            Some(value) => (value.clone(), true),
            None => return Err(format!("Required pricing dimension '{name}' is missing.")),
        },
    };
    let resolved = policy.aliases.get(&raw).cloned().unwrap_or(raw);
    if !policy.allowed.contains(&resolved) {
        return Err(format!(
            "Pricing dimension '{name}' has unsupported value '{resolved}'."
        ));
    }
    Ok((resolved, assumed))
}

fn resolve_explicit_dimension(
    name: &str,
    policy: &DimensionPolicy,
    raw: &str,
) -> Result<String, String> {
    let raw = raw.trim().to_ascii_lowercase();
    if is_explicitly_unresolved_dimension_value(&raw) {
        return Err(format!(
            "Pricing dimension '{name}' is explicitly unresolved ('{raw}'); it cannot be used as an attested value."
        ));
    }
    let resolved = policy.aliases.get(&raw).cloned().unwrap_or(raw);
    if !policy.allowed.contains(&resolved) {
        return Err(format!(
            "Pricing dimension '{name}' has unsupported value '{resolved}'."
        ));
    }
    Ok(resolved)
}

fn is_explicitly_unresolved_dimension_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "unknown" | "not_available" | "not-available" | "unavailable" | "n/a" | "null"
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn validate_sha256_text(value: &str) -> Result<(), PricingError> {
    let value = value.trim();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PricingError::Verification(
            "SHA-256 values must contain exactly 64 hexadecimal characters".to_string(),
        ));
    }
    Ok(())
}

fn verify_sha256(bytes: &[u8], expected_sha256: &str) -> Result<(), PricingError> {
    let calculated = sha256_hex(bytes);
    if calculated.eq_ignore_ascii_case(expected_sha256.trim()) {
        Ok(())
    } else {
        Err(PricingError::ChecksumMismatch {
            expected: expected_sha256.trim().to_ascii_lowercase(),
            calculated,
        })
    }
}

fn require_verified(catalog: &PriceCatalog) -> Result<(), PricingError> {
    let report = catalog.verify();
    if report.is_valid() {
        Ok(())
    } else {
        let message = report
            .issues
            .iter()
            .filter(|issue| issue.severity == VerificationSeverity::Error)
            .map(|issue| format!("{}: {}", issue.code, issue.message))
            .collect::<Vec<_>>()
            .join("; ");
        Err(PricingError::Verification(message))
    }
}

fn verify_document(document: &CatalogDocument) -> VerificationReport {
    let mut report = VerificationReport::default();
    let mut error = |code: &str, message: String| {
        report.issues.push(VerificationIssue {
            severity: VerificationSeverity::Error,
            code: code.to_string(),
            message,
        });
    };

    if document.schema_version != CATALOG_SCHEMA_VERSION {
        error(
            "schema_version",
            format!(
                "expected schema version {CATALOG_SCHEMA_VERSION}, got {}",
                document.schema_version
            ),
        );
    }
    if document.revision.trim().is_empty() {
        error("empty_revision", "revision must not be empty".to_string());
    }
    if document.verified_at < document.published_at {
        error(
            "verification_before_publication",
            "verified_at precedes published_at".to_string(),
        );
    }
    if document.stale_after_days == 0 {
        error(
            "zero_stale_window",
            "stale_after_days must be positive".to_string(),
        );
    }

    let mut collection_limit_exceeded = false;
    for (name, count) in [
        ("sources", document.sources.len()),
        ("aliases", document.aliases.len()),
        ("rates", document.rates.len()),
        ("modifiers", document.modifiers.len()),
    ] {
        if count > MAX_CATALOG_COLLECTION_ENTRIES {
            collection_limit_exceeded = true;
            error(
                "collection_limit",
                format!(
                    "catalog {name} contains {count} entries; the safety limit is {MAX_CATALOG_COLLECTION_ENTRIES}"
                ),
            );
        }
    }
    if collection_limit_exceeded {
        report.valid = false;
        return report;
    }

    let mut source_ids = HashSet::new();
    for source in &document.sources {
        if !source_ids.insert(source.id.as_str()) {
            error(
                "duplicate_source",
                format!("duplicate source id '{}'", source.id),
            );
        }
        if !source.url.starts_with("https://") {
            error(
                "source_url",
                format!("source '{}' does not use https", source.id),
            );
        }
    }

    let source_set: HashSet<&str> = document.sources.iter().map(|s| s.id.as_str()).collect();
    let mut record_ids = HashSet::new();
    for alias in &document.aliases {
        verify_record_id(&mut record_ids, &alias.id, &mut error);
        verify_interval(&alias.id, &alias.interval, &mut error);
        verify_sources(&alias.id, &alias.source_ids, &source_set, &mut error);
        if alias.provider.trim().is_empty()
            || alias.raw_model.trim().is_empty()
            || alias.canonical_model.trim().is_empty()
        {
            error(
                "empty_alias_field",
                format!("alias '{}' has an empty identity field", alias.id),
            );
        }
    }
    for (index, left) in document.aliases.iter().enumerate() {
        for right in document.aliases.iter().skip(index + 1) {
            if left.provider == right.provider
                && left.raw_model == right.raw_model
                && left.interval.overlaps(&right.interval)
            {
                error(
                    "overlapping_alias",
                    format!("aliases '{}' and '{}' overlap", left.id, right.id),
                );
            }
        }
    }

    for rule in &document.rates {
        verify_record_id(&mut record_ids, &rule.id, &mut error);
        verify_interval(&rule.id, &rule.interval, &mut error);
        verify_sources(&rule.id, &rule.source_ids, &source_set, &mut error);
        if rule.unit_scale == 0 {
            error(
                "zero_unit_scale",
                format!("rate '{}' has unit_scale 0", rule.id),
            );
        }
        if rule
            .max_input_tokens_exclusive
            .zip(rule.min_input_tokens)
            .is_some_and(|(max, min)| max <= min)
        {
            error(
                "invalid_context_interval",
                format!("rate '{}' has an empty input-token interval", rule.id),
            );
        }
        for (component, value) in rule.rates.iter_named() {
            if value.is_some_and(|value| value.0 < Decimal::ZERO) {
                error(
                    "negative_rate",
                    format!("rate '{}' component '{component}' is negative", rule.id),
                );
            }
            if value.is_some_and(|value| !catalog_decimal_magnitude_is_safe(value.0)) {
                error(
                    "rate_magnitude",
                    format!(
                        "rate '{}' component '{component}' exceeds the supported decimal magnitude",
                        rule.id
                    ),
                );
            }
        }
        if !rule.rates.any_cache_write_rate() && rule.cache_write_observation_required {
            // This is valid for Codex credits: the missing official cache-write
            // column is itself material, so no warning is emitted.
        }
        for (name, rate) in &rule.tool_rates {
            if rate.per_units == 0 {
                error(
                    "zero_tool_scale",
                    format!("rate '{}' tool '{name}' has per_units 0", rule.id),
                );
            }
            if rate.rate.0 < Decimal::ZERO || (rate.rate.0 == Decimal::ZERO && !rate.explicit_free)
            {
                error(
                    "invalid_tool_rate",
                    format!(
                        "rate '{}' tool '{name}' must be positive or explicitly free",
                        rule.id
                    ),
                );
            }
            if !catalog_decimal_magnitude_is_safe(rate.rate.0) {
                error(
                    "tool_rate_magnitude",
                    format!(
                        "rate '{}' tool '{name}' exceeds the supported decimal magnitude",
                        rule.id
                    ),
                );
            }
        }
        for (name, policy) in &rule.dimension_policies {
            if policy.allowed.is_empty() {
                error(
                    "empty_dimension_policy",
                    format!(
                        "rate '{}' dimension '{name}' has no allowed values",
                        rule.id
                    ),
                );
            }
            let unique: BTreeSet<&str> = policy.allowed.iter().map(String::as_str).collect();
            if unique.len() != policy.allowed.len() {
                error(
                    "duplicate_dimension_value",
                    format!("rate '{}' dimension '{name}' repeats a value", rule.id),
                );
            }
            if policy
                .default
                .as_ref()
                .is_some_and(|value| !policy.allowed.contains(value))
            {
                error(
                    "invalid_dimension_default",
                    format!(
                        "rate '{}' dimension '{name}' has an invalid default",
                        rule.id
                    ),
                );
            }
            for target in policy.aliases.values() {
                if !policy.allowed.contains(target) {
                    error(
                        "invalid_dimension_alias",
                        format!(
                            "rate '{}' dimension '{name}' aliases to unsupported '{target}'",
                            rule.id
                        ),
                    );
                }
            }
        }
    }

    for (index, left) in document.rates.iter().enumerate() {
        for right in document.rates.iter().skip(index + 1) {
            if left.provider == right.provider
                && left.canonical_model == right.canonical_model
                && left.kind == right.kind
                && left.interval.overlaps(&right.interval)
                && left.context_overlaps(right)
            {
                error(
                    "overlapping_rate",
                    format!("rates '{}' and '{}' overlap", left.id, right.id),
                );
            }
        }
    }

    for modifier in &document.modifiers {
        verify_record_id(&mut record_ids, &modifier.id, &mut error);
        verify_interval(&modifier.id, &modifier.interval, &mut error);
        verify_sources(&modifier.id, &modifier.source_ids, &source_set, &mut error);
        if modifier.multiplier.0 <= Decimal::ZERO {
            error(
                "invalid_modifier",
                format!("modifier '{}' is not positive", modifier.id),
            );
        }
        if !catalog_decimal_magnitude_is_safe(modifier.multiplier.0) {
            error(
                "modifier_magnitude",
                format!(
                    "modifier '{}' exceeds the supported decimal magnitude",
                    modifier.id
                ),
            );
        }
        if modifier.selectors.is_empty() {
            error(
                "empty_modifier_selector",
                format!("modifier '{}' has no selectors", modifier.id),
            );
        }
        let target_rules: Vec<&RateRule> = document
            .rates
            .iter()
            .filter(|rule| {
                rule.provider == modifier.provider
                    && rule.canonical_model == modifier.canonical_model
                    && rule.kind == modifier.kind
                    && rule.interval.overlaps(&modifier.interval)
            })
            .collect();
        if target_rules.is_empty() {
            error(
                "orphan_modifier",
                format!("modifier '{}' has no overlapping base rate", modifier.id),
            );
        }
        for (dimension, selected) in &modifier.selectors {
            if !target_rules.iter().any(|rule| {
                rule.dimension_policies
                    .get(dimension)
                    .is_some_and(|policy| policy.allowed.contains(selected))
            }) {
                error(
                    "unreachable_modifier",
                    format!(
                        "modifier '{}' selects unsupported {dimension}='{selected}'",
                        modifier.id
                    ),
                );
            }
        }
    }
    for (index, left) in document.modifiers.iter().enumerate() {
        for right in document.modifiers.iter().skip(index + 1) {
            if left.provider == right.provider
                && left.canonical_model == right.canonical_model
                && left.kind == right.kind
                && left.scope == right.scope
                && left.selectors == right.selectors
                && left.interval.overlaps(&right.interval)
            {
                error(
                    "duplicate_modifier",
                    format!(
                        "modifiers '{}' and '{}' would stack twice",
                        left.id, right.id
                    ),
                );
            }
        }
    }

    report.valid = !report
        .issues
        .iter()
        .any(|issue| issue.severity == VerificationSeverity::Error);
    report
}

fn catalog_decimal_magnitude_is_safe(value: Decimal) -> bool {
    let maximum = Decimal::from(MAX_CATALOG_DECIMAL_MAGNITUDE);
    value >= -maximum && value <= maximum
}

fn checked_pricing_product(left: Decimal, right: Decimal) -> Option<Decimal> {
    let value = left.checked_mul(right)?;
    let maximum = Decimal::from(MAX_PRICING_RESULT_MAGNITUDE);
    (value >= -maximum && value <= maximum).then_some(value)
}

fn checked_pricing_sum(left: Decimal, right: Decimal) -> Option<Decimal> {
    pricing_magnitude_is_safe(left.checked_add(right)?)
}

fn checked_pricing_difference(left: Decimal, right: Decimal) -> Option<Decimal> {
    pricing_magnitude_is_safe(left.checked_sub(right)?)
}

fn pricing_magnitude_is_safe(value: Decimal) -> Option<Decimal> {
    let maximum = Decimal::from(MAX_PRICING_RESULT_MAGNITUDE);
    (value >= -maximum && value <= maximum).then_some(value)
}

fn checked_component_cost(quantity: u64, rate: Decimal, scale: u64) -> Option<Decimal> {
    checked_pricing_product(Decimal::from(quantity), rate)?
        .checked_div(Decimal::from(scale))
        .and_then(pricing_magnitude_is_safe)
}

fn verify_record_id<'a>(
    ids: &mut HashSet<&'a str>,
    id: &'a str,
    error: &mut impl FnMut(&str, String),
) {
    if id.trim().is_empty() {
        error("empty_record_id", "record id must not be empty".to_string());
    } else if !ids.insert(id) {
        error("duplicate_record_id", format!("duplicate record id '{id}'"));
    }
}

fn verify_interval(id: &str, interval: &EffectiveInterval, error: &mut impl FnMut(&str, String)) {
    if interval
        .effective_to
        .is_some_and(|end| end <= interval.effective_from)
    {
        error(
            "invalid_effective_interval",
            format!("record '{id}' has an empty effective interval"),
        );
    }
}

fn verify_sources(
    id: &str,
    sources: &[String],
    source_set: &HashSet<&str>,
    error: &mut impl FnMut(&str, String),
) {
    if sources.is_empty() {
        error(
            "missing_source",
            format!("record '{id}' has no official source"),
        );
    }
    for source in sources {
        if !source_set.contains(source.as_str()) {
            error(
                "unknown_source",
                format!("record '{id}' references unknown source '{source}'"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn when() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap()
    }

    fn event(client: Client, provider: &str, model: &str, usage: UsageVector) -> CanonicalEvent {
        CanonicalEvent {
            event_id: "event".to_string(),
            event_key: "key".to_string(),
            client,
            session_id: "session".to_string(),
            provider_message_id: None,
            occurred_at: when(),
            raw_model: model.to_string(),
            provider: provider.to_string(),
            usage,
            dimensions: PricingDimensions {
                auth_mode: (client == Client::OpenaiCodex).then(|| "chatgpt".to_string()),
                speed: (client == Client::OpenaiCodex).then(|| "standard".to_string()),
                cache_write_data_complete: Some(true),
                ..Default::default()
            },
            quality: crate::model::UsageQuality::Exact,
            coverage: crate::model::CoverageStatus::CompleteKnown,
            source_count: 1,
            warnings: Vec::new(),
        }
    }

    fn d(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn candidate_bytes(revision: &str) -> Vec<u8> {
        let engine = PricingEngine::bundled().unwrap();
        let mut document: serde_json::Value =
            serde_json::from_slice(engine.catalog().raw_bytes()).unwrap();
        document["revision"] = serde_json::Value::String(revision.to_string());
        document["published_at"] = serde_json::Value::String("2026-07-11T00:00:00Z".to_string());
        document["verified_at"] = serde_json::Value::String("2026-07-11T00:00:00Z".to_string());
        serde_json::to_vec_pretty(&document).unwrap()
    }

    #[test]
    fn bundled_catalog_is_valid_and_current() {
        let engine = PricingEngine::bundled().unwrap();
        assert!(engine.verify().is_valid());
        assert_eq!(engine.catalog().schema_version(), 1);
        assert_eq!(engine.status_at(when()).freshness, CatalogFreshness::Fresh);
        assert_eq!(
            engine
                .status_at(Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap())
                .freshness,
            CatalogFreshness::Stale
        );
    }

    #[test]
    fn claude_rates_all_token_classes_and_tools_exactly() {
        let engine = PricingEngine::bundled().unwrap();
        let usage = UsageVector {
            input_tokens_total: 4_000_000,
            input_tokens_uncached: 1_000_000,
            input_tokens_cached: 1_000_000,
            cache_write_5m_tokens: 1_000_000,
            cache_write_1h_tokens: 1_000_000,
            output_tokens_total: 1_000_000,
            web_search_requests: 1_000,
            web_fetch_requests: 3,
            ..Default::default()
        };
        let estimate = engine.estimate_event(&event(
            Client::ClaudeCode,
            "anthropic",
            "claude-fable-5",
            usage,
        ));
        assert_eq!(estimate.status, EstimateStatus::Priced);
        assert_eq!(estimate.api_equivalent_usd, Some(d("103.5")));
        assert_eq!(estimate.provider_units, None);
    }

    #[test]
    fn codex_returns_usd_and_credit_estimates() {
        let engine = PricingEngine::bundled().unwrap();
        let usage = UsageVector {
            input_tokens_total: 2_000_000,
            input_tokens_uncached: 1_000_000,
            input_tokens_cached: 1_000_000,
            output_tokens_total: 1_000_000,
            ..Default::default()
        };
        let estimate =
            engine.estimate_event(&event(Client::OpenaiCodex, "openai", "gpt-5.6-sol", usage));
        assert_eq!(estimate.status, EstimateStatus::Priced);
        assert_eq!(estimate.api_equivalent_usd, Some(d("35.5")));
        assert_eq!(estimate.provider_units, Some(d("887.5")));
        assert_eq!(
            estimate.provider_unit_name.as_deref(),
            Some("Codex credits")
        );
        assert!(estimate.pricing_evidence.iter().any(|record| {
            record.record_id == "openai-gpt-5-6-sol-usd-standard"
                && record.effective_from == Utc.with_ymd_and_hms(2026, 7, 9, 0, 0, 0).unwrap()
                && record
                    .sources
                    .iter()
                    .all(|source| source.url.starts_with("https://"))
        }));
    }

    #[test]
    fn release_date_boundary_prices_yesterday_without_backcasting() {
        let engine = PricingEngine::bundled().unwrap();
        let usage = UsageVector {
            input_tokens_total: 1_000,
            input_tokens_uncached: 1_000,
            output_tokens_total: 100,
            ..Default::default()
        };
        let mut item = event(Client::OpenaiCodex, "openai", "gpt-5.6-sol", usage);
        item.occurred_at = Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap();
        assert_eq!(engine.estimate_event(&item).status, EstimateStatus::Priced);

        item.occurred_at = Utc.with_ymd_and_hms(2026, 7, 8, 23, 59, 59).unwrap();
        let before_release = engine.estimate_event(&item);
        assert_eq!(before_release.status, EstimateStatus::Unpriced);
        assert_eq!(before_release.api_equivalent_usd, None);
        assert_eq!(before_release.provider_units, None);
    }

    #[test]
    fn codex_credit_estimate_requires_a_matching_auth_route() {
        let engine = PricingEngine::bundled().unwrap();
        let usage = UsageVector {
            input_tokens_total: 1_000_000,
            input_tokens_uncached: 1_000_000,
            output_tokens_total: 1_000_000,
            ..Default::default()
        };

        let mut api_key = event(Client::OpenaiCodex, "openai", "gpt-5.6-sol", usage.clone());
        api_key.dimensions.auth_mode = Some("api_key".to_string());
        let api_key_estimate = engine.estimate_event(&api_key);
        assert_eq!(api_key_estimate.status, EstimateStatus::Priced);
        assert_eq!(api_key_estimate.provider_units, None);
        assert_eq!(api_key_estimate.provider_unit_name, None);
        assert!(
            api_key_estimate
                .explanation
                .iter()
                .any(|line| line.contains("API-key authentication"))
        );

        let mut unknown = event(Client::OpenaiCodex, "openai", "gpt-5.6-sol", usage);
        unknown.dimensions.auth_mode = None;
        let unknown_estimate = engine.estimate_event(&unknown);
        assert_eq!(unknown_estimate.status, EstimateStatus::Partial);
        assert_eq!(unknown_estimate.provider_units, None);
        assert!(
            unknown_estimate
                .missing_components
                .iter()
                .any(|missing| { missing.reason.contains("auth_mode") })
        );

        let mut unknown_speed = event(
            Client::OpenaiCodex,
            "openai",
            "gpt-5.6-sol",
            UsageVector {
                input_tokens_total: 1_000,
                input_tokens_uncached: 1_000,
                ..Default::default()
            },
        );
        unknown_speed.dimensions.speed = None;
        unknown_speed.dimensions.service_tier = None;
        let unknown_speed_estimate = engine.estimate_event(&unknown_speed);
        assert_eq!(unknown_speed_estimate.status, EstimateStatus::Partial);
        assert_eq!(unknown_speed_estimate.provider_units, None);
        assert!(
            unknown_speed_estimate
                .missing_components
                .iter()
                .any(|missing| missing.reason.contains("speed"))
        );
    }

    #[test]
    fn generic_openai_cache_write_rate_is_supported() {
        let engine = PricingEngine::bundled().unwrap();
        let usage = UsageVector {
            input_tokens_total: 3_000_000,
            input_tokens_uncached: 1_000_000,
            input_tokens_cached: 1_000_000,
            cache_write_unknown_tokens: 1_000_000,
            output_tokens_total: 1_000_000,
            ..Default::default()
        };
        let estimate =
            engine.estimate_event(&event(Client::OpenaiCodex, "openai", "gpt-5.6", usage));
        assert_eq!(estimate.api_equivalent_usd, Some(d("41.75")));
        assert_eq!(estimate.status, EstimateStatus::Priced);
        assert_eq!(estimate.provider_units, Some(d("1012.5")));
        assert_eq!(estimate.provider_unit_measure.status, MeasureStatus::Exact);
        assert!(estimate.provider_unit_measure.missing_components.is_empty());
    }

    #[test]
    fn incomplete_codex_cache_writes_bound_usd_but_not_credits() {
        let engine = PricingEngine::bundled().unwrap();
        let mut item = event(
            Client::OpenaiCodex,
            "openai",
            "gpt-5.6-sol",
            UsageVector {
                input_tokens_total: 2_000_000,
                input_tokens_uncached: 1_000_000,
                input_tokens_cached: 1_000_000,
                ..Default::default()
            },
        );
        item.dimensions.service_tier = Some("standard".to_string());
        item.dimensions.cache_write_data_complete = Some(false);

        let estimate = engine.estimate_event(&item);
        assert_eq!(
            estimate.api_equivalent_usd_measure.status,
            MeasureStatus::Bounded
        );
        assert_eq!(
            estimate.api_equivalent_usd_measure.lower_bound,
            Some(d("5.5"))
        );
        assert_eq!(
            estimate.api_equivalent_usd_measure.upper_bound,
            Some(d("6.75"))
        );
        assert_eq!(estimate.provider_unit_measure.status, MeasureStatus::Exact);
        assert_eq!(estimate.provider_unit_measure.lower_bound, Some(d("137.5")));
        assert_eq!(estimate.provider_unit_measure.upper_bound, Some(d("137.5")));
        assert_eq!(estimate.provider_units, Some(d("137.5")));

        item.dimensions.input_subset_accounting_consistent = Some(false);
        let invalid_invariant = engine.estimate_event(&item);
        assert_eq!(
            invalid_invariant.api_equivalent_usd_measure.status,
            MeasureStatus::Partial
        );
        assert_eq!(
            invalid_invariant.api_equivalent_usd_measure.upper_bound,
            None
        );
    }

    #[test]
    fn inferred_current_profile_auth_never_claims_exact_historical_credits() {
        let engine = PricingEngine::bundled().unwrap();
        let mut item = event(
            Client::OpenaiCodex,
            "openai",
            "gpt-5.6-sol",
            UsageVector {
                input_tokens_total: 1_000_000,
                input_tokens_uncached: 1_000_000,
                ..Default::default()
            },
        );
        item.dimensions.service_tier = Some("standard".to_string());
        item.dimensions.auth_mode_provenance =
            Some(DimensionValueProvenance::CurrentProfileInferred);

        let estimate = engine.estimate_event(&item);
        assert_eq!(estimate.status, EstimateStatus::Partial);
        assert_eq!(estimate.provider_units, None);
        assert_eq!(
            estimate.provider_unit_measure.status,
            MeasureStatus::Bounded
        );
        assert_eq!(
            estimate.provider_unit_measure.lower_bound,
            Some(Decimal::ZERO)
        );
        assert_eq!(estimate.provider_unit_measure.upper_bound, Some(d("125")));
        assert!(
            estimate
                .provider_unit_measure
                .dimension_evidence
                .iter()
                .any(|evidence| {
                    evidence.dimension == "auth_mode"
                        && evidence.provenance == AssumptionProvenance::CurrentProfileInferred
                        && evidence.confidence == PricingDimensionConfidence::Inferred
                })
        );

        item.dimensions.auth_mode_provenance = None;
        item.warnings.push(
            "auth mode was inferred from the current Codex profile; a historical session may have used a different route"
                .to_string(),
        );
        let legacy = engine.estimate_event(&item);
        assert_eq!(legacy.provider_unit_measure.status, MeasureStatus::Bounded);
        assert_eq!(legacy.provider_units, None);

        item.warnings.clear();
        item.dimensions.auth_mode = Some("api_key".to_string());
        item.dimensions.auth_mode_provenance =
            Some(DimensionValueProvenance::CurrentProfileInferred);
        let inferred_api_key = engine.estimate_event(&item);
        assert_eq!(
            inferred_api_key.provider_unit_measure.status,
            MeasureStatus::Bounded
        );
        assert_eq!(
            inferred_api_key.provider_unit_measure.lower_bound,
            Some(Decimal::ZERO)
        );
        assert_eq!(
            inferred_api_key.provider_unit_measure.upper_bound,
            Some(d("125"))
        );
    }

    #[test]
    fn fast_codex_credits_use_documented_multiplier() {
        let engine = PricingEngine::bundled().unwrap();
        let mut item = event(
            Client::OpenaiCodex,
            "openai",
            "gpt-5.4",
            UsageVector {
                input_tokens_total: 2_000_000,
                input_tokens_uncached: 1_000_000,
                input_tokens_cached: 1_000_000,
                output_tokens_total: 1_000_000,
                ..Default::default()
            },
        );
        // Older Codex envelopes expose Fast mode through service_tier rather
        // than the dedicated speed dimension.
        item.dimensions.speed = None;
        item.dimensions.service_tier = Some("fast".to_string());
        let estimate = engine.estimate_event(&item);
        assert_eq!(estimate.provider_units, Some(d("887.5")));
        assert!(
            estimate
                .matched_rate_ids
                .iter()
                .any(|id| id == "openai-gpt-5-4-credits-fast")
        );
    }

    #[test]
    fn partial_and_unknown_are_never_exact_zero() {
        let engine = PricingEngine::bundled().unwrap();
        let partial = engine.estimate_event(&event(
            Client::OpenaiCodex,
            "openai",
            "gpt-5.5",
            UsageVector {
                input_tokens_total: 10,
                cache_write_unknown_tokens: 10,
                ..Default::default()
            },
        ));
        assert_eq!(partial.status, EstimateStatus::Partial);
        assert_eq!(partial.api_equivalent_usd, None);

        let unknown = engine.estimate_event(&event(
            Client::OpenaiCodex,
            "openai",
            "deepseek-v4-pro",
            UsageVector {
                output_tokens_total: 10,
                ..Default::default()
            },
        ));
        assert_eq!(unknown.status, EstimateStatus::Unpriced);
        assert_eq!(unknown.api_equivalent_usd, None);
        assert_eq!(unknown.provider_units, None);
    }

    #[test]
    fn low_context_rule_does_not_guess_above_official_boundary() {
        let engine = PricingEngine::bundled().unwrap();
        let estimate = engine.estimate_event(&event(
            Client::OpenaiCodex,
            "openai",
            "gpt-5.5",
            UsageVector {
                input_tokens_total: 272_000,
                input_tokens_uncached: 272_000,
                ..Default::default()
            },
        ));
        assert_eq!(estimate.api_equivalent_usd, None);
        assert!(
            estimate
                .explanation
                .iter()
                .any(|line| line.contains("272000"))
        );
    }

    #[test]
    fn one_token_uses_decimal_not_binary_float() {
        let engine = PricingEngine::bundled().unwrap();
        let estimate = engine.estimate_event(&event(
            Client::OpenaiCodex,
            "openai",
            "gpt-5.4",
            UsageVector {
                input_tokens_total: 1,
                input_tokens_uncached: 1,
                ..Default::default()
            },
        ));
        assert_eq!(estimate.api_equivalent_usd, Some(d("0.0000025")));
    }

    #[test]
    fn incomplete_cache_observation_is_partial_not_zero() {
        let engine = PricingEngine::bundled().unwrap();
        let mut item = event(
            Client::ClaudeCode,
            "anthropic",
            "claude-sonnet-4-6",
            UsageVector {
                input_tokens_total: 1_000,
                input_tokens_uncached: 1_000,
                ..Default::default()
            },
        );
        item.dimensions.cache_write_data_complete = Some(false);
        let estimate = engine.estimate_event(&item);
        assert_eq!(estimate.status, EstimateStatus::Partial);
        assert_eq!(estimate.api_equivalent_usd, None);
        assert_eq!(estimate.known_api_equivalent_usd, Some(d("0.003")));
    }

    #[test]
    fn partial_with_only_an_explicitly_free_known_component_has_no_zero_subtotal() {
        let engine = PricingEngine::bundled().unwrap();
        let mut item = event(
            Client::ClaudeCode,
            "anthropic",
            "claude-sonnet-4-6",
            UsageVector {
                web_fetch_requests: 1,
                ..Default::default()
            },
        );
        item.dimensions.cache_write_data_complete = Some(false);
        let estimate = engine.estimate_event(&item);
        assert_eq!(estimate.status, EstimateStatus::Partial);
        assert_eq!(estimate.api_equivalent_usd, None);
        assert_eq!(estimate.known_api_equivalent_usd, None);
    }

    #[test]
    fn explicit_unavailable_sentinel_is_not_silently_treated_as_global() {
        let engine = PricingEngine::bundled().unwrap();
        let mut item = event(
            Client::ClaudeCode,
            "anthropic",
            "claude-fable-5",
            UsageVector {
                input_tokens_total: 1_000_000,
                input_tokens_uncached: 1_000_000,
                output_tokens_total: 1_000_000,
                ..Default::default()
            },
        );
        item.dimensions.inference_geo = Some("not_available".to_string());

        let exact = engine.estimate_event(&item);
        assert_eq!(exact.status, EstimateStatus::Unpriced);
        assert_eq!(exact.api_equivalent_usd, None);
        assert!(exact.missing_components.iter().any(|missing| {
            missing.reason.contains("explicitly unresolved")
                && missing.reason.contains("inference_geo")
        }));
    }

    #[test]
    fn fable_unavailable_geo_enumerates_documented_global_and_us_bounds() {
        let engine = PricingEngine::bundled().unwrap();
        let mut item = event(
            Client::ClaudeCode,
            "anthropic",
            "claude-fable-5",
            UsageVector {
                input_tokens_total: 1_000_000,
                input_tokens_uncached: 1_000_000,
                output_tokens_total: 1_000_000,
                ..Default::default()
            },
        );
        item.dimensions.inference_geo = Some("unknown".to_string());

        let estimate = engine.estimate_event_range(&item);
        let range = estimate.api_equivalent_usd;
        assert_eq!(range.status, PriceRangeStatus::Bounded);
        assert_eq!(range.lower_bound, Some(d("60")));
        assert_eq!(range.upper_bound, Some(d("66.0")));
        assert_eq!(range.scenarios.len(), 2);
        assert!(range.scenarios.iter().any(|scenario| {
            scenario.dimensions.get("inference_geo").map(String::as_str) == Some("global")
                && scenario.amount == Some(d("60"))
        }));
        assert!(range.scenarios.iter().any(|scenario| {
            scenario.dimensions.get("inference_geo").map(String::as_str) == Some("us")
                && scenario.amount == Some(d("66.0"))
                && scenario
                    .matched_rate_ids
                    .iter()
                    .any(|id| id == "anthropic-claude-fable-5-us-geo")
        }));
        assert!(range.scenarios.iter().all(|scenario| {
            scenario.assumptions.iter().any(|assumption| {
                assumption.dimension == "inference_geo"
                    && assumption.provenance == AssumptionProvenance::CatalogAllowedScenario
                    && assumption
                        .sources
                        .iter()
                        .any(|source| source.id == "anthropic-pricing")
            })
        }));
    }

    #[test]
    fn range_explores_missing_dimensions_while_single_estimate_keeps_catalog_default() {
        let engine = PricingEngine::bundled().unwrap();
        let item = event(
            Client::ClaudeCode,
            "anthropic",
            "claude-fable-5",
            UsageVector {
                input_tokens_total: 1_000_000,
                input_tokens_uncached: 1_000_000,
                output_tokens_total: 1_000_000,
                ..Default::default()
            },
        );
        let exact = engine.estimate_event(&item);
        assert_eq!(exact.api_equivalent_usd, Some(d("60")));
        assert!(exact.pricing_assumptions.iter().any(|assumption| {
            assumption.dimension == "inference_geo"
                && assumption.provenance == AssumptionProvenance::CatalogDefault
        }));

        let range = engine.estimate_event_range(&item).api_equivalent_usd;
        assert_eq!(range.status, PriceRangeStatus::Bounded);
        assert_eq!(range.lower_bound, Some(d("60")));
        assert_eq!(range.upper_bound, Some(d("66.0")));
    }

    #[test]
    fn bounded_user_attestation_resolves_range_and_preserves_provenance() {
        let override_value = PricingDimensionOverride {
            id: "fable-global-july".to_string(),
            provider: "anthropic".to_string(),
            canonical_model: "claude-fable-5".to_string(),
            dimension: "inference_geo".to_string(),
            value: "global".to_string(),
            effective_from: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
            effective_to: Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
            attested_at: Utc.with_ymd_and_hms(2026, 7, 10, 13, 0, 0).unwrap(),
            note: Some("checked organization routing setting".to_string()),
        };
        let engine = PricingEngine::bundled()
            .unwrap()
            .with_dimension_overrides(vec![override_value.clone()])
            .unwrap();
        let mut item = event(
            Client::ClaudeCode,
            "anthropic",
            "claude-fable-5",
            UsageVector {
                input_tokens_total: 1_000_000,
                input_tokens_uncached: 1_000_000,
                output_tokens_total: 1_000_000,
                ..Default::default()
            },
        );
        item.dimensions.inference_geo = Some("not_available".to_string());

        let exact = engine.estimate_event(&item);
        assert_eq!(exact.status, EstimateStatus::Priced);
        assert_eq!(exact.api_equivalent_usd, Some(d("60")));
        let evidence = exact
            .pricing_assumptions
            .iter()
            .find(|assumption| assumption.provenance == AssumptionProvenance::UserAttestedOverride)
            .unwrap();
        assert_eq!(evidence.override_id.as_deref(), Some("fable-global-july"));
        assert_eq!(evidence.effective_from, Some(override_value.effective_from));
        assert_eq!(evidence.effective_to, Some(override_value.effective_to));
        assert_eq!(evidence.attested_at, Some(override_value.attested_at));
        assert_eq!(evidence.note, override_value.note);
        assert_eq!(evidence.confidence, PricingDimensionConfidence::Attested);
        assert!(evidence.sources.is_empty());

        let range = engine.estimate_event_range(&item).api_equivalent_usd;
        assert_eq!(range.status, PriceRangeStatus::Exact);
        assert_eq!(range.lower_bound, Some(d("60")));
        assert_eq!(range.upper_bound, Some(d("60")));
        assert_eq!(range.scenarios.len(), 1);

        item.occurred_at = override_value.effective_to;
        assert_eq!(
            engine.estimate_event(&item).status,
            EstimateStatus::Unpriced
        );
        assert_eq!(
            engine.estimate_event_range(&item).api_equivalent_usd.status,
            PriceRangeStatus::Bounded
        );
    }

    #[test]
    fn override_must_use_a_documented_value_and_target() {
        let base = PricingDimensionOverride {
            id: "invalid".to_string(),
            provider: "anthropic".to_string(),
            canonical_model: "claude-fable-5".to_string(),
            dimension: "inference_geo".to_string(),
            value: "mars".to_string(),
            effective_from: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
            effective_to: Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
            attested_at: when(),
            note: None,
        };
        assert!(matches!(
            PricingEngine::bundled()
                .unwrap()
                .with_dimension_overrides(vec![base.clone()]),
            Err(PricingError::InvalidOverride(_))
        ));

        let mut unknown_target = base;
        unknown_target.value = "global".to_string();
        unknown_target.canonical_model = "unknown-model".to_string();
        assert!(matches!(
            PricingEngine::bundled()
                .unwrap()
                .with_dimension_overrides(vec![unknown_target]),
            Err(PricingError::InvalidOverride(_))
        ));
    }

    #[test]
    fn checksum_hook_rejects_tampering_before_update_checks() {
        let engine = PricingEngine::bundled().unwrap();
        let result = engine.validate_candidate_with_checksum(b"{}", "00");
        assert!(matches!(result, Err(PricingError::ChecksumMismatch { .. })));
    }

    #[test]
    fn catalog_rejects_extreme_decimal_magnitudes_and_collection_counts() {
        let mut extreme: serde_json::Value = serde_json::from_slice(BUNDLED_CATALOG).unwrap();
        extreme["rates"][0]["rates"]["input"] = serde_json::Value::String("1000000001".to_string());
        let catalog = PriceCatalog::parse(&serde_json::to_vec(&extreme).unwrap()).unwrap();
        let report = catalog.verify();
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "rate_magnitude")
        );

        let mut crowded: serde_json::Value = serde_json::from_slice(BUNDLED_CATALOG).unwrap();
        let template = crowded["aliases"][0].clone();
        crowded["aliases"] = serde_json::Value::Array(
            (0..=MAX_CATALOG_COLLECTION_ENTRIES)
                .map(|index| {
                    let mut alias = template.clone();
                    alias["id"] = serde_json::Value::String(format!("bounded-alias-{index}"));
                    alias["raw_model"] =
                        serde_json::Value::String(format!("bounded-model-{index}"));
                    alias
                })
                .collect(),
        );
        let catalog = PriceCatalog::parse(&serde_json::to_vec(&crowded).unwrap()).unwrap();
        let report = catalog.verify();
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "collection_limit")
        );
    }

    #[test]
    fn pricing_product_limit_returns_unpriced_instead_of_panicking() {
        let mut catalog = PriceCatalog::parse(BUNDLED_CATALOG).unwrap();
        let matching = catalog
            .document
            .rates
            .iter_mut()
            .find(|rule| {
                rule.provider == "openai"
                    && rule.canonical_model == "gpt-5.6-sol"
                    && rule.kind == RateKind::UsdApiEquivalent
            })
            .unwrap();
        matching.unit_scale = 1;
        matching.rates.input = Some(ExactDecimal(Decimal::from(MAX_CATALOG_DECIMAL_MAGNITUDE)));
        let engine = PricingEngine {
            catalog,
            dimension_overrides: Vec::new(),
        };
        let item = event(
            Client::OpenaiCodex,
            "openai",
            "gpt-5.6-sol",
            UsageVector {
                input_tokens_total: u64::MAX,
                input_tokens_uncached: u64::MAX,
                ..Default::default()
            },
        );
        let estimate = engine.estimate_event(&item);
        assert_eq!(estimate.status, EstimateStatus::Unpriced);
        assert!(
            estimate
                .explanation
                .iter()
                .any(|line| line.contains("supported decimal range"))
        );
    }

    #[test]
    fn current_catalog_is_not_accepted_as_an_update() {
        let engine = PricingEngine::bundled().unwrap();
        let result = engine.validate_candidate(engine.catalog().raw_bytes());
        assert!(matches!(result, Err(PricingError::UpdateRejected(_))));
    }

    #[test]
    fn atomic_install_retains_previous_and_new_revisions() {
        let directory = tempfile::tempdir().unwrap();
        let active_path = directory.path().join("prices.json");
        let engine = PricingEngine::bundled().unwrap();
        fs::write(&active_path, engine.catalog().raw_bytes()).unwrap();
        let bytes = candidate_bytes("2026-07-11.1");
        let checksum = sha256_hex(&bytes);

        let receipt = engine
            .install_candidate(&bytes, Some(&checksum), &active_path)
            .unwrap();

        assert_eq!(fs::read(&active_path).unwrap(), bytes);
        assert_eq!(receipt.installed_revision, "2026-07-11.1");
        assert_eq!(receipt.retained_revisions.len(), 2);
        let retained = PricingEngine::retained_revisions(&active_path).unwrap();
        assert_eq!(retained.len(), 2);
        assert!(
            retained
                .iter()
                .any(|item| item.revision == engine.catalog().revision())
        );
        assert!(retained.iter().any(|item| item.revision == "2026-07-11.1"));
        assert!(retained.iter().all(|item| {
            item.path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&item.revision) && name.contains(&item.sha256))
        }));
    }

    #[test]
    fn invalid_catalog_cannot_replace_active_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let active_path = directory.path().join("prices.json");
        let engine = PricingEngine::bundled().unwrap();
        let original = engine.catalog().raw_bytes().to_vec();
        fs::write(&active_path, &original).unwrap();
        let mut invalid: serde_json::Value =
            serde_json::from_slice(&candidate_bytes("2026-07-11.2")).unwrap();
        invalid["schema_version"] = serde_json::Value::from(999);
        let invalid_bytes = serde_json::to_vec_pretty(&invalid).unwrap();
        let parsed_but_unverified = PriceCatalog::parse(&invalid_bytes).unwrap();

        let result = PricingEngine::save_candidate(&parsed_but_unverified, &active_path);

        assert!(matches!(result, Err(PricingError::Verification(_))));
        assert_eq!(fs::read(&active_path).unwrap(), original);
        assert!(!PricingEngine::history_dir(&active_path).exists());
    }

    #[test]
    fn installation_failure_before_commit_leaves_active_bytes_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let active_path = directory.path().join("prices.json");
        let engine = PricingEngine::bundled().unwrap();
        let original = engine.catalog().raw_bytes().to_vec();
        fs::write(&active_path, &original).unwrap();
        // A file where the history directory must be makes snapshot retention
        // fail deterministically before the atomic active-file commit.
        fs::write(PricingEngine::history_dir(&active_path), b"blocked").unwrap();
        let bytes = candidate_bytes("2026-07-11.3");

        let result = engine.install_candidate(&bytes, None, &active_path);

        assert!(matches!(result, Err(PricingError::Write { .. })));
        assert_eq!(fs::read(&active_path).unwrap(), original);
    }

    #[test]
    fn bad_checksum_cannot_create_or_replace_catalog_files() {
        let directory = tempfile::tempdir().unwrap();
        let active_path = directory.path().join("prices.json");
        let engine = PricingEngine::bundled().unwrap();
        let original = engine.catalog().raw_bytes().to_vec();
        fs::write(&active_path, &original).unwrap();
        let bytes = candidate_bytes("2026-07-11.4");

        let result = engine.install_candidate(&bytes, Some("00"), &active_path);

        assert!(matches!(result, Err(PricingError::ChecksumMismatch { .. })));
        assert_eq!(fs::read(&active_path).unwrap(), original);
        assert!(!PricingEngine::history_dir(&active_path).exists());
    }
}
