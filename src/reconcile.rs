//! Optional provider reconciliation.
//!
//! Provider exports are retained as a separate evidence layer. They never
//! become local usage observations and therefore cannot change scan totals.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::db::Ledger;
use crate::model::{CanonicalEvent, Client};

pub const RECONCILIATION_SCHEMA_VERSION: &str = "token-ledger.reconciliation.v1";
pub const RECONCILIATION_REPORT_SCHEMA_VERSION: &str = "token-ledger.reconciliation-report.v1";
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;
const BOUNDARY_TOLERANCE_SECONDS: i64 = 60 * 60;

type NormalizedBucketKey = (
    String,
    DateTime<Utc>,
    DateTime<Utc>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Auto,
    CanonicalJson,
    CanonicalCsv,
    Openai,
    Anthropic,
}

impl FromStr for ImportFormat {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "auto" => Ok(Self::Auto),
            "json" | "canonical-json" => Ok(Self::CanonicalJson),
            "csv" | "canonical-csv" => Ok(Self::CanonicalCsv),
            "openai" | "openai-organization" => Ok(Self::Openai),
            "anthropic" | "anthropic-admin" => Ok(Self::Anthropic),
            _ => anyhow::bail!(
                "unsupported reconciliation format; use auto, canonical-json, canonical-csv, openai, or anthropic"
            ),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationCounters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_uncached: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_cached: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_5m_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_unknown_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

impl ReconciliationCounters {
    fn any_reported(&self) -> bool {
        self.request_count.is_some()
            || self.input_tokens_uncached.is_some()
            || self.input_tokens_cached.is_some()
            || self.cache_write_5m_tokens.is_some()
            || self.cache_write_1h_tokens.is_some()
            || self.cache_write_unknown_tokens.is_some()
            || self.output_tokens.is_some()
    }

    fn all_reported_zero(&self) -> bool {
        self.any_reported()
            && [
                self.request_count,
                self.input_tokens_uncached,
                self.input_tokens_cached,
                self.cache_write_5m_tokens,
                self.cache_write_1h_tokens,
                self.cache_write_unknown_tokens,
                self.output_tokens,
            ]
            .into_iter()
            .flatten()
            .all(|value| value == 0)
    }

    fn zero_for_reported_fields(&self) -> Self {
        Self {
            request_count: self.request_count.map(|_| 0),
            input_tokens_uncached: self.input_tokens_uncached.map(|_| 0),
            input_tokens_cached: self.input_tokens_cached.map(|_| 0),
            cache_write_5m_tokens: self.cache_write_5m_tokens.map(|_| 0),
            cache_write_1h_tokens: self.cache_write_1h_tokens.map(|_| 0),
            cache_write_unknown_tokens: self.cache_write_unknown_tokens.map(|_| 0),
            output_tokens: self.output_tokens.map(|_| 0),
        }
    }

    fn checked_add_assign(&mut self, other: &Self) -> Result<()> {
        combine_complete(&mut self.request_count, other.request_count)?;
        combine_complete(&mut self.input_tokens_uncached, other.input_tokens_uncached)?;
        combine_complete(&mut self.input_tokens_cached, other.input_tokens_cached)?;
        combine_complete(&mut self.cache_write_5m_tokens, other.cache_write_5m_tokens)?;
        combine_complete(&mut self.cache_write_1h_tokens, other.cache_write_1h_tokens)?;
        combine_complete(
            &mut self.cache_write_unknown_tokens,
            other.cache_write_unknown_tokens,
        )?;
        combine_complete(&mut self.output_tokens, other.output_tokens)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReconciliationRouting {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_route: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReconciliationBucketDraft {
    pub source_kind: String,
    pub bucket_start: DateTime<Utc>,
    pub bucket_end: DateTime<Utc>,
    pub provider: String,
    pub model: Option<String>,
    pub counters: ReconciliationCounters,
    pub provider_metered_usd: Option<Decimal>,
    pub routing: ReconciliationRouting,
}

#[derive(Debug, Clone)]
pub struct ParsedReconciliationImport {
    pub content_digest: String,
    pub source_kind: String,
    pub adapter: String,
    pub provider: Option<String>,
    pub byte_count: u64,
    pub buckets: Vec<ReconciliationBucketDraft>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationImportRecord {
    pub id: i64,
    pub content_digest: String,
    pub source_kind: String,
    pub adapter: String,
    pub provider: Option<String>,
    pub imported_at: DateTime<Utc>,
    pub byte_count: u64,
    pub bucket_count: u64,
}

#[derive(Debug, Clone)]
pub struct StoredReconciliationBucket {
    pub import_id: i64,
    pub import_digest: String,
    pub imported_at: DateTime<Utc>,
    pub source_kind: String,
    pub bucket_start: DateTime<Utc>,
    pub bucket_end: DateTime<Utc>,
    pub provider: String,
    pub model: Option<String>,
    pub counters: ReconciliationCounters,
    pub provider_metered_usd: Option<Decimal>,
    pub routing: ReconciliationRouting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReceipt {
    pub content_digest: String,
    pub source_kind: String,
    pub adapter: String,
    pub provider: Option<String>,
    pub bucket_count: u64,
    pub imported: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationStatus {
    pub schema_version: String,
    pub import_count: u64,
    pub bucket_count: u64,
    pub providers: Vec<String>,
    pub earliest_bucket_start: Option<DateTime<Utc>>,
    pub latest_bucket_end: Option<DateTime<Utc>>,
    pub latest_imports: Vec<ReconciliationImportRecord>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationClassification {
    Matched,
    LocalOnly,
    ProviderOnly,
    CounterMismatch,
    TimeBoundary,
    RouteUnknown,
}

impl ReconciliationClassification {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::LocalOnly => "local_only",
            Self::ProviderOnly => "provider_only",
            Self::CounterMismatch => "counter_mismatch",
            Self::TimeBoundary => "time_boundary",
            Self::RouteUnknown => "route_unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationSide {
    pub counters: ReconciliationCounters,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_metered_usd: Option<Decimal>,
    pub routing: ReconciliationRouting,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReconciliationDeltas {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_count: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_uncached: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_cached: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_5m_tokens: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_tokens: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_unknown_tokens: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i128>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationRow {
    pub bucket_start_utc: DateTime<Utc>,
    pub bucket_end_utc: DateTime<Utc>,
    pub provider: String,
    pub model: Option<String>,
    pub classification: ReconciliationClassification,
    pub local: Option<ReconciliationSide>,
    pub provider_evidence: Option<ReconciliationSide>,
    pub deltas_provider_minus_local: ReconciliationDeltas,
    pub evidence_digest: Option<String>,
    pub source_kind: Option<String>,
    pub explanation: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReconciliationSummary {
    pub matched: u64,
    pub local_only: u64,
    pub provider_only: u64,
    pub counter_mismatch: u64,
    pub time_boundary: u64,
    pub route_unknown: u64,
}

impl ReconciliationSummary {
    fn record(&mut self, classification: ReconciliationClassification) {
        match classification {
            ReconciliationClassification::Matched => self.matched += 1,
            ReconciliationClassification::LocalOnly => self.local_only += 1,
            ReconciliationClassification::ProviderOnly => self.provider_only += 1,
            ReconciliationClassification::CounterMismatch => self.counter_mismatch += 1,
            ReconciliationClassification::TimeBoundary => self.time_boundary += 1,
            ReconciliationClassification::RouteUnknown => self.route_unknown += 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationReport {
    pub schema_version: String,
    pub generated_at: DateTime<Utc>,
    pub start_utc: Option<DateTime<Utc>>,
    pub end_utc: Option<DateTime<Utc>>,
    pub timezone: String,
    pub import_count: u64,
    pub selected_bucket_count: u64,
    pub summary: ReconciliationSummary,
    pub rows: Vec<ReconciliationRow>,
    pub coverage: Vec<String>,
    pub limitations: Vec<String>,
}

pub fn import_path(
    ledger: &mut Ledger,
    path: &Path,
    format: ImportFormat,
) -> Result<ImportReceipt> {
    let file = File::open(path).context("failed to open reconciliation import")?;
    let size = file
        .metadata()
        .context("failed to inspect reconciliation import")?
        .len();
    if size > MAX_IMPORT_BYTES {
        anyhow::bail!("reconciliation import exceeds the 64 MiB safety limit");
    }
    let mut bytes = Vec::with_capacity(size as usize);
    BufReader::new(file)
        .take(MAX_IMPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to read reconciliation import")?;
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        anyhow::bail!("reconciliation import exceeds the 64 MiB safety limit");
    }
    let parsed = parse_import(&bytes, format)?;
    ledger.store_reconciliation_import(&parsed)
}

pub fn parse_import(bytes: &[u8], format: ImportFormat) -> Result<ParsedReconciliationImport> {
    if bytes.is_empty() {
        anyhow::bail!("reconciliation import is empty");
    }
    let digest = hex::encode(Sha256::digest(bytes));
    let selected = if format == ImportFormat::Auto {
        match bytes
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace())
        {
            Some(b'{') | Some(b'[') => ImportFormat::CanonicalJson,
            _ => ImportFormat::CanonicalCsv,
        }
    } else {
        format
    };

    let (adapter, mut buckets) = match selected {
        ImportFormat::CanonicalCsv => ("canonical_csv", parse_canonical_csv(bytes)?),
        ImportFormat::CanonicalJson => {
            let document: Value = serde_json::from_slice(bytes)
                .context("invalid reconciliation JSON (content was not retained)")?;
            if is_canonical_json(&document) {
                ("canonical_json", parse_canonical_json(&document)?)
            } else if looks_anthropic(&document) {
                ("anthropic_admin_api", parse_anthropic_json(&document)?)
            } else {
                ("openai_organization_api", parse_openai_json(&document)?)
            }
        }
        ImportFormat::Openai => {
            let document: Value = serde_json::from_slice(bytes)
                .context("invalid OpenAI reconciliation JSON (content was not retained)")?;
            ("openai_organization_api", parse_openai_json(&document)?)
        }
        ImportFormat::Anthropic => {
            let document: Value = serde_json::from_slice(bytes)
                .context("invalid Anthropic reconciliation JSON (content was not retained)")?;
            ("anthropic_admin_api", parse_anthropic_json(&document)?)
        }
        ImportFormat::Auto => unreachable!(),
    };
    normalize_buckets(&mut buckets)?;
    if buckets.is_empty() {
        anyhow::bail!("reconciliation import contained no supported usage or cost buckets");
    }
    let source_kinds: BTreeSet<_> = buckets
        .iter()
        .map(|bucket| bucket.source_kind.as_str())
        .collect();
    let providers: BTreeSet<_> = buckets
        .iter()
        .map(|bucket| bucket.provider.as_str())
        .collect();
    Ok(ParsedReconciliationImport {
        content_digest: digest,
        source_kind: if source_kinds.len() == 1 {
            source_kinds.into_iter().next().unwrap().to_string()
        } else {
            "mixed_provider_export".to_string()
        },
        adapter: adapter.to_string(),
        provider: (providers.len() == 1).then(|| providers.into_iter().next().unwrap().to_string()),
        byte_count: bytes.len() as u64,
        buckets,
    })
}

pub fn status(ledger: &Ledger) -> Result<ReconciliationStatus> {
    let imports = ledger.reconciliation_imports()?;
    let buckets = ledger.reconciliation_buckets()?;
    let providers = buckets
        .iter()
        .map(|bucket| bucket.provider.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let earliest_bucket_start = buckets.iter().map(|bucket| bucket.bucket_start).min();
    let latest_bucket_end = buckets.iter().map(|bucket| bucket.bucket_end).max();
    let mut latest_by_kind: BTreeMap<String, ReconciliationImportRecord> = BTreeMap::new();
    for import in &imports {
        latest_by_kind
            .entry(import.source_kind.clone())
            .and_modify(|existing| {
                if import.id > existing.id {
                    *existing = import.clone();
                }
            })
            .or_insert_with(|| import.clone());
    }
    Ok(ReconciliationStatus {
        schema_version: RECONCILIATION_SCHEMA_VERSION.to_string(),
        import_count: imports.len() as u64,
        bucket_count: buckets.len() as u64,
        providers,
        earliest_bucket_start,
        latest_bucket_end,
        latest_imports: latest_by_kind.into_values().collect(),
        limitations: reconciliation_limitations(),
    })
}

pub fn report(
    ledger: &Ledger,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    timezone: Tz,
) -> Result<ReconciliationReport> {
    if let (Some(start), Some(end)) = (start, end)
        && end <= start
    {
        anyhow::bail!("reconciliation report end must be after start");
    }
    let imports = ledger.reconciliation_imports()?;
    let stored = ledger.reconciliation_buckets()?;
    let selected = select_latest_buckets(stored, start, end);
    let events = ledger.canonical_events(start, end)?;
    let mut covered_events = HashSet::new();
    let mut rows = Vec::new();

    for bucket in &selected {
        let exact_indices = matching_event_indices(&events, bucket, false);
        let mut local = aggregate_local(&events, &exact_indices)?;
        let provider_has_usage = bucket.counters.any_reported();
        if provider_has_usage {
            covered_events.extend(exact_indices.iter().copied());
        }
        let provider_side = ReconciliationSide {
            counters: bucket.counters.clone(),
            provider_metered_usd: bucket.provider_metered_usd,
            routing: bucket.routing.clone(),
        };
        let classification;
        let explanation;

        if exact_indices.is_empty() {
            if bucket.counters.all_reported_zero() {
                local = Some(ReconciliationSide {
                    counters: bucket.counters.zero_for_reported_fields(),
                    provider_metered_usd: None,
                    routing: ReconciliationRouting::default(),
                });
                classification = ReconciliationClassification::Matched;
                explanation =
                    "the provider reports zero for every supplied counter and no local activity is present"
                        .to_string();
            } else {
                let boundary_indices = matching_event_indices(&events, bucket, true);
                if !boundary_indices.is_empty() && provider_has_usage {
                    local = aggregate_local(&events, &boundary_indices)?;
                    classification = ReconciliationClassification::TimeBoundary;
                    explanation = "matching local activity exists only within one hour of an exclusive provider bucket boundary".to_string();
                } else {
                    classification = ReconciliationClassification::ProviderOnly;
                    explanation = if provider_has_usage {
                        "the provider export reports usage but no matching local event falls inside this bucket".to_string()
                    } else {
                        "the provider supplied a cost-only bucket with no comparable local token counters".to_string()
                    };
                }
            }
        } else if !provider_has_usage {
            classification = ReconciliationClassification::RouteUnknown;
            explanation =
                "the provider bucket has cost or metadata but no token counters to compare"
                    .to_string();
        } else {
            let local_ref = local.as_ref().expect("non-empty local aggregate");
            let deltas = counter_deltas(&local_ref.counters, &provider_side.counters);
            if deltas_have_mismatch(&deltas) {
                classification = ReconciliationClassification::CounterMismatch;
                explanation =
                    "one or more provider counters differ from the matching local aggregate"
                        .to_string();
            } else if routing_unknown(bucket, &events, &exact_indices)? {
                classification = ReconciliationClassification::RouteUnknown;
                explanation = "token counters align, but routing dimensions are absent or cannot be uniquely matched".to_string();
            } else {
                classification = ReconciliationClassification::Matched;
                explanation =
                    "all provider-supplied counters match the local aggregate".to_string();
            }
        }
        let deltas = counter_deltas(
            &local
                .as_ref()
                .map(|side| side.counters.clone())
                .unwrap_or_default(),
            &provider_side.counters,
        );
        rows.push(ReconciliationRow {
            bucket_start_utc: bucket.bucket_start,
            bucket_end_utc: bucket.bucket_end,
            provider: bucket.provider.clone(),
            model: bucket.model.clone(),
            classification,
            local,
            provider_evidence: Some(provider_side),
            deltas_provider_minus_local: deltas,
            evidence_digest: Some(bucket.import_digest.clone()),
            source_kind: Some(bucket.source_kind.clone()),
            explanation,
        });
    }

    rows.extend(local_only_rows(&events, &covered_events, timezone)?);
    rows.sort_by(|left, right| {
        left.bucket_start_utc
            .cmp(&right.bucket_start_utc)
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.model.cmp(&right.model))
            .then_with(|| left.classification.cmp(&right.classification))
    });
    let mut summary = ReconciliationSummary::default();
    for row in &rows {
        summary.record(row.classification);
    }

    Ok(ReconciliationReport {
        schema_version: RECONCILIATION_REPORT_SCHEMA_VERSION.to_string(),
        generated_at: Utc::now(),
        start_utc: start,
        end_utc: end,
        timezone: timezone.to_string(),
        import_count: imports.len() as u64,
        selected_bucket_count: selected.len() as u64,
        summary,
        rows,
        coverage: vec![
            "The latest imported snapshot for each source kind is selected; older imports remain immutable history.".to_string(),
            "Provider buckets use exact half-open UTC intervals [start, end); when a report window is supplied, only fully contained provider buckets are compared.".to_string(),
            "Local-only rows use calendar days in the selected timezone, including 23-hour and 25-hour DST days.".to_string(),
            "Only provider-supplied counters are compared; omitted fields remain unknown rather than zero.".to_string(),
        ],
        limitations: reconciliation_limitations(),
    })
}

fn reconciliation_limitations() -> Vec<String> {
    vec![
        "Provider reconciliation is an independent evidence layer and never creates, replaces, or edits local usage observations.".to_string(),
        "Organization API exports do not establish which usage belongs to an individual ChatGPT or Claude subscription.".to_string(),
        "Provider-metered cost is retained only when the export explicitly supplies a USD amount; it is not recomputed or presented as an invoice.".to_string(),
        "Imported organization, workspace, project, user, and API-key identifiers are neither echoed nor persisted.".to_string(),
        "Absence from an imported export is not proof that provider usage or billing was zero.".to_string(),
    ]
}

fn select_latest_buckets(
    buckets: Vec<StoredReconciliationBucket>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
) -> Vec<StoredReconciliationBucket> {
    let latest_import_by_kind = buckets.iter().fold(HashMap::new(), |mut map, bucket| {
        map.entry(bucket.source_kind.clone())
            .and_modify(|id: &mut i64| *id = (*id).max(bucket.import_id))
            .or_insert(bucket.import_id);
        map
    });
    buckets
        .into_iter()
        .filter(|bucket| {
            latest_import_by_kind.get(&bucket.source_kind) == Some(&bucket.import_id)
                && start.is_none_or(|start| bucket.bucket_start >= start)
                && end.is_none_or(|end| bucket.bucket_end <= end)
        })
        .collect()
}

fn matching_event_indices(
    events: &[CanonicalEvent],
    bucket: &StoredReconciliationBucket,
    boundary_only: bool,
) -> Vec<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| provider_for_event(event) == bucket.provider)
        .filter(|(_, event)| {
            bucket
                .model
                .as_ref()
                .is_none_or(|model| model.eq_ignore_ascii_case(&event.raw_model))
        })
        .filter(|(_, event)| {
            let exact =
                event.occurred_at >= bucket.bucket_start && event.occurred_at < bucket.bucket_end;
            if !boundary_only {
                exact
            } else if exact {
                false
            } else {
                let before_start =
                    bucket.bucket_start - Duration::seconds(BOUNDARY_TOLERANCE_SECONDS);
                let after_end = bucket.bucket_end + Duration::seconds(BOUNDARY_TOLERANCE_SECONDS);
                event.occurred_at >= before_start && event.occurred_at <= after_end
            }
        })
        .map(|(index, _)| index)
        .collect()
}

fn aggregate_local(
    events: &[CanonicalEvent],
    indices: &[usize],
) -> Result<Option<ReconciliationSide>> {
    if indices.is_empty() {
        return Ok(None);
    }
    let mut counters = ReconciliationCounters {
        request_count: Some(0),
        input_tokens_uncached: Some(0),
        input_tokens_cached: Some(0),
        cache_write_5m_tokens: None,
        cache_write_1h_tokens: None,
        cache_write_unknown_tokens: None,
        output_tokens: Some(0),
    };
    let mut geos = BTreeSet::new();
    let mut tiers = BTreeSet::new();
    let mut routes = BTreeSet::new();
    for index in indices {
        let event = &events[*index];
        add_optional(&mut counters.request_count, Some(1))?;
        add_optional(
            &mut counters.input_tokens_uncached,
            Some(event.usage.input_tokens_uncached),
        )?;
        add_optional(
            &mut counters.input_tokens_cached,
            Some(event.usage.input_tokens_cached),
        )?;
        if event.usage.cache_write_5m_tokens > 0
            || event.dimensions.cache_write_data_complete == Some(true)
        {
            add_optional(
                &mut counters.cache_write_5m_tokens,
                Some(event.usage.cache_write_5m_tokens),
            )?;
        }
        if event.usage.cache_write_1h_tokens > 0
            || event.dimensions.cache_write_data_complete == Some(true)
        {
            add_optional(
                &mut counters.cache_write_1h_tokens,
                Some(event.usage.cache_write_1h_tokens),
            )?;
        }
        if event.usage.cache_write_unknown_tokens > 0
            || event.dimensions.cache_write_data_complete == Some(true)
        {
            add_optional(
                &mut counters.cache_write_unknown_tokens,
                Some(event.usage.cache_write_unknown_tokens),
            )?;
        }
        add_optional(
            &mut counters.output_tokens,
            Some(event.usage.output_tokens_total),
        )?;
        if let Some(value) = event.dimensions.inference_geo.as_deref() {
            geos.insert(value.to_string());
        }
        if let Some(value) = event.dimensions.service_tier.as_deref() {
            tiers.insert(value.to_string());
        }
        if let Some(value) = event.dimensions.provider_route.as_deref() {
            routes.insert(value.to_string());
        }
    }
    Ok(Some(ReconciliationSide {
        counters,
        provider_metered_usd: None,
        routing: ReconciliationRouting {
            inference_geo: unique_value(geos),
            service_tier: unique_value(tiers),
            provider_route: unique_value(routes),
        },
    }))
}

fn local_only_rows(
    events: &[CanonicalEvent],
    covered: &HashSet<usize>,
    timezone: Tz,
) -> Result<Vec<ReconciliationRow>> {
    let mut groups: BTreeMap<(NaiveDate, String, String), Vec<usize>> = BTreeMap::new();
    for (index, event) in events.iter().enumerate() {
        if covered.contains(&index) {
            continue;
        }
        groups
            .entry((
                event.occurred_at.with_timezone(&timezone).date_naive(),
                provider_for_event(event),
                event.raw_model.clone(),
            ))
            .or_default()
            .push(index);
    }
    groups
        .into_iter()
        .map(|((date, provider, model), indices)| {
            let start = local_midnight(date, timezone)?;
            let end = local_midnight(
                date.checked_add_days(chrono::Days::new(1))
                    .context("local reconciliation date overflow")?,
                timezone,
            )?;
            Ok(ReconciliationRow {
                bucket_start_utc: start,
                bucket_end_utc: end,
                provider,
                model: Some(model),
                classification: ReconciliationClassification::LocalOnly,
                local: aggregate_local(events, &indices)?,
                provider_evidence: None,
                deltas_provider_minus_local: ReconciliationDeltas::default(),
                evidence_digest: None,
                source_kind: None,
                explanation:
                    "local activity is not covered by the selected provider token-usage buckets"
                        .to_string(),
            })
        })
        .collect()
}

fn local_midnight(date: NaiveDate, timezone: Tz) -> Result<DateTime<Utc>> {
    let naive = date
        .and_hms_opt(0, 0, 0)
        .context("invalid local midnight")?;
    let local = match timezone.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(early, _) => early,
        LocalResult::None => anyhow::bail!("selected timezone has no local midnight for {date}"),
    };
    Ok(local.with_timezone(&Utc))
}

fn routing_unknown(
    bucket: &StoredReconciliationBucket,
    events: &[CanonicalEvent],
    indices: &[usize],
) -> Result<bool> {
    let local = aggregate_local(events, indices)?
        .map(|value| value.routing)
        .unwrap_or_default();
    Ok(routing_field_unknown(
        bucket.routing.inference_geo.as_deref(),
        local.inference_geo.as_deref(),
    ) || routing_field_unknown(
        bucket.routing.service_tier.as_deref(),
        local.service_tier.as_deref(),
    ) || routing_field_unknown(
        bucket.routing.provider_route.as_deref(),
        local.provider_route.as_deref(),
    ) || (bucket.provider == "anthropic"
        && bucket.routing.inference_geo.is_none()
        && local.inference_geo.is_none()))
}

fn routing_field_unknown(provider: Option<&str>, local: Option<&str>) -> bool {
    match (provider, local) {
        (None, None) => false,
        (Some(left), Some(right)) => !left.eq_ignore_ascii_case(right),
        _ => true,
    }
}

fn counter_deltas(
    local: &ReconciliationCounters,
    provider: &ReconciliationCounters,
) -> ReconciliationDeltas {
    ReconciliationDeltas {
        request_count: delta(local.request_count, provider.request_count),
        input_tokens_uncached: delta(local.input_tokens_uncached, provider.input_tokens_uncached),
        input_tokens_cached: delta(local.input_tokens_cached, provider.input_tokens_cached),
        cache_write_5m_tokens: delta(local.cache_write_5m_tokens, provider.cache_write_5m_tokens),
        cache_write_1h_tokens: delta(local.cache_write_1h_tokens, provider.cache_write_1h_tokens),
        cache_write_unknown_tokens: delta(
            local.cache_write_unknown_tokens,
            provider.cache_write_unknown_tokens,
        ),
        output_tokens: delta(local.output_tokens, provider.output_tokens),
    }
}

fn deltas_have_mismatch(deltas: &ReconciliationDeltas) -> bool {
    [
        deltas.request_count,
        deltas.input_tokens_uncached,
        deltas.input_tokens_cached,
        deltas.cache_write_5m_tokens,
        deltas.cache_write_1h_tokens,
        deltas.cache_write_unknown_tokens,
        deltas.output_tokens,
    ]
    .into_iter()
    .flatten()
    .any(|value| value != 0)
}

fn delta(local: Option<u64>, provider: Option<u64>) -> Option<i128> {
    Some(provider? as i128 - local? as i128)
}

fn provider_for_event(event: &CanonicalEvent) -> String {
    let normalized = normalize_provider(&event.provider);
    if normalized == "unknown" {
        match event.client {
            Client::ClaudeCode => "anthropic".to_string(),
            Client::OpenaiCodex => "openai".to_string(),
        }
    } else {
        normalized
    }
}

fn unique_value(values: BTreeSet<String>) -> Option<String> {
    (values.len() == 1).then(|| values.into_iter().next().unwrap())
}

fn add_optional(target: &mut Option<u64>, value: Option<u64>) -> Result<()> {
    if let Some(value) = value {
        *target = Some(
            target
                .unwrap_or(0)
                .checked_add(value)
                .context("local reconciliation counters exceed the u64 accounting limit")?,
        );
    }
    Ok(())
}

fn combine_complete(target: &mut Option<u64>, value: Option<u64>) -> Result<()> {
    *target = match (*target, value) {
        (Some(left), Some(right)) => Some(
            left.checked_add(right)
                .context("reconciliation import counters exceed the u64 accounting limit")?,
        ),
        _ => None,
    };
    Ok(())
}

fn is_canonical_json(value: &Value) -> bool {
    value
        .get("schema_version")
        .and_then(Value::as_str)
        .is_some_and(|version| version.starts_with("token-ledger.reconciliation"))
        || value.get("buckets").is_some()
}

fn looks_anthropic(value: &Value) -> bool {
    value
        .get("usage")
        .is_some_and(|usage| usage.to_string().contains("uncached_input_tokens"))
        || value
            .get("data")
            .and_then(Value::as_array)
            .and_then(|data| data.first())
            .is_some_and(|item| item.get("starting_at").is_some())
}

fn parse_canonical_json(document: &Value) -> Result<Vec<ReconciliationBucketDraft>> {
    if let Some(version) = document.get("schema_version").and_then(Value::as_str)
        && version != RECONCILIATION_SCHEMA_VERSION
    {
        anyhow::bail!(
            "unsupported canonical reconciliation schema; expected {RECONCILIATION_SCHEMA_VERSION}"
        );
    }
    let root_kind = optional_string(document, &["source_kind"])
        .and_then(|value| safe_identifier(&value, 64))
        .unwrap_or_else(|| "canonical_json".to_string());
    let values = document
        .get("buckets")
        .and_then(Value::as_array)
        .or_else(|| document.as_array())
        .context("canonical reconciliation JSON requires a buckets array")?;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| parse_canonical_bucket(value, &root_kind, index + 1))
        .collect()
}

fn parse_canonical_bucket(
    value: &Value,
    root_kind: &str,
    ordinal: usize,
) -> Result<ReconciliationBucketDraft> {
    let start = required_datetime(
        value,
        &["bucket_start", "bucket_start_utc", "start", "start_time"],
    )
    .with_context(|| format!("canonical bucket {ordinal} has an invalid start"))?;
    let end = optional_datetime(value, &["bucket_end", "bucket_end_utc", "end", "end_time"])?
        .unwrap_or(start + Duration::days(1));
    let provider = required_safe(value, &["provider"], 32, "provider", ordinal)?;
    let usage = value.get("usage").unwrap_or(value);
    let source_kind = optional_string(value, &["source_kind"])
        .and_then(|text| safe_identifier(&text, 64))
        .unwrap_or_else(|| root_kind.to_string());
    let mut counters = counters_from_value(usage)?;
    if !std::ptr::eq(usage, value) {
        let root_counters = counters_from_value(value)?;
        fill_missing_counters(&mut counters, &root_counters);
    }
    Ok(ReconciliationBucketDraft {
        source_kind,
        bucket_start: start,
        bucket_end: end,
        provider: normalize_provider(&provider),
        model: optional_string(value, &["model"]).and_then(|text| safe_identifier(&text, 128)),
        counters,
        provider_metered_usd: usd_from_value(value)?,
        routing: routing_from_value(value),
    })
}

fn parse_canonical_csv(bytes: &[u8]) -> Result<Vec<ReconciliationBucketDraft>> {
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(bytes);
    let headers = reader
        .headers()
        .context("invalid canonical reconciliation CSV header")?
        .iter()
        .map(normalize_header)
        .collect::<Vec<_>>();
    let mut buckets = Vec::new();
    for (index, record) in reader.records().enumerate() {
        let record = record.map_err(|_| {
            anyhow::anyhow!("invalid canonical reconciliation CSV record {}", index + 2)
        })?;
        let mut object = serde_json::Map::new();
        for (header, value) in headers.iter().zip(record.iter()) {
            if !value.trim().is_empty() {
                object.insert(header.clone(), Value::String(value.trim().to_string()));
            }
        }
        buckets.push(parse_canonical_bucket(
            &Value::Object(object),
            "canonical_csv",
            index + 1,
        )?);
    }
    Ok(buckets)
}

fn normalize_header(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace([' ', '-'], "_")
}

fn parse_openai_json(document: &Value) -> Result<Vec<ReconciliationBucketDraft>> {
    let mut buckets = Vec::new();
    if let Some(usage) = document.get("usage") {
        parse_openai_page(usage, Some(false), &mut buckets)?;
    }
    if let Some(costs) = document.get("costs") {
        parse_openai_page(costs, Some(true), &mut buckets)?;
    }
    if buckets.is_empty() {
        parse_openai_page(document, None, &mut buckets)?;
    }
    Ok(buckets)
}

fn parse_openai_page(
    page: &Value,
    force_cost: Option<bool>,
    buckets: &mut Vec<ReconciliationBucketDraft>,
) -> Result<()> {
    let data = page
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| page.as_array())
        .context("OpenAI export requires a data array")?;
    for bucket in data {
        let start = required_datetime(bucket, &["start_time", "starting_at", "bucket_start"])
            .context("OpenAI bucket has an invalid start")?;
        let end = optional_datetime(bucket, &["end_time", "ending_at", "bucket_end"])?
            .unwrap_or(start + Duration::days(1));
        let results = bucket
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| vec![bucket.clone()]);
        for result in results {
            let cost = force_cost.unwrap_or_else(|| is_cost_value(&result));
            if cost {
                buckets.push(ReconciliationBucketDraft {
                    source_kind: "openai_organization_costs".to_string(),
                    bucket_start: start,
                    bucket_end: end,
                    provider: "openai".to_string(),
                    model: optional_string(&result, &["model"])
                        .and_then(|text| safe_identifier(&text, 128)),
                    counters: ReconciliationCounters::default(),
                    provider_metered_usd: usd_from_value(&result)?,
                    routing: routing_from_value(&result),
                });
            } else {
                let mut counters = counters_from_value(&result)?;
                let input_total = optional_u64(&result, &["input_tokens"])?;
                if counters.input_tokens_uncached.is_none()
                    && let (Some(total), Some(cached)) = (input_total, counters.input_tokens_cached)
                {
                    counters.input_tokens_uncached = Some(total.saturating_sub(cached));
                }
                buckets.push(ReconciliationBucketDraft {
                    source_kind: "openai_organization_usage".to_string(),
                    bucket_start: start,
                    bucket_end: end,
                    provider: "openai".to_string(),
                    model: optional_string(&result, &["model"])
                        .and_then(|text| safe_identifier(&text, 128)),
                    counters,
                    provider_metered_usd: None,
                    routing: routing_from_value(&result),
                });
            }
        }
    }
    Ok(())
}

fn parse_anthropic_json(document: &Value) -> Result<Vec<ReconciliationBucketDraft>> {
    let mut buckets = Vec::new();
    if let Some(usage) = document.get("usage") {
        parse_anthropic_page(usage, Some(false), &mut buckets)?;
    }
    if let Some(costs) = document.get("costs") {
        parse_anthropic_page(costs, Some(true), &mut buckets)?;
    }
    if buckets.is_empty() {
        parse_anthropic_page(document, None, &mut buckets)?;
    }
    Ok(buckets)
}

fn parse_anthropic_page(
    page: &Value,
    force_cost: Option<bool>,
    buckets: &mut Vec<ReconciliationBucketDraft>,
) -> Result<()> {
    let data = page
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| page.as_array())
        .context("Anthropic export requires a data array")?;
    for result in data {
        let start = required_datetime(result, &["starting_at", "start_time", "bucket_start"])
            .context("Anthropic bucket has an invalid start")?;
        let end = optional_datetime(result, &["ending_at", "end_time", "bucket_end"])?
            .unwrap_or(start + Duration::days(1));
        let cost = force_cost.unwrap_or_else(|| is_cost_value(result));
        if cost {
            buckets.push(ReconciliationBucketDraft {
                source_kind: "anthropic_admin_costs".to_string(),
                bucket_start: start,
                bucket_end: end,
                provider: "anthropic".to_string(),
                model: optional_string(result, &["model"])
                    .and_then(|text| safe_identifier(&text, 128)),
                counters: ReconciliationCounters::default(),
                provider_metered_usd: usd_from_value(result)?,
                routing: routing_from_value(result),
            });
        } else {
            let mut counters = counters_from_value(result)?;
            if let Some(cache_creation) = result.get("cache_creation") {
                counters.cache_write_5m_tokens = optional_u64(
                    cache_creation,
                    &["ephemeral_5m_input_tokens", "cache_write_5m_tokens"],
                )?;
                counters.cache_write_1h_tokens = optional_u64(
                    cache_creation,
                    &["ephemeral_1h_input_tokens", "cache_write_1h_tokens"],
                )?;
            }
            buckets.push(ReconciliationBucketDraft {
                source_kind: "anthropic_admin_usage".to_string(),
                bucket_start: start,
                bucket_end: end,
                provider: "anthropic".to_string(),
                model: optional_string(result, &["model"])
                    .and_then(|text| safe_identifier(&text, 128)),
                counters,
                provider_metered_usd: None,
                routing: routing_from_value(result),
            });
        }
    }
    Ok(())
}

fn counters_from_value(value: &Value) -> Result<ReconciliationCounters> {
    Ok(ReconciliationCounters {
        request_count: optional_u64(value, &["request_count", "requests", "num_model_requests"])?,
        input_tokens_uncached: optional_u64(
            value,
            &[
                "input_tokens_uncached",
                "uncached_input_tokens",
                "uncached_tokens",
            ],
        )?,
        input_tokens_cached: optional_u64(
            value,
            &[
                "input_tokens_cached",
                "cached_input_tokens",
                "input_cached_tokens",
                "cache_read_input_tokens",
            ],
        )?,
        cache_write_5m_tokens: optional_u64(
            value,
            &["cache_write_5m_tokens", "ephemeral_5m_input_tokens"],
        )?,
        cache_write_1h_tokens: optional_u64(
            value,
            &["cache_write_1h_tokens", "ephemeral_1h_input_tokens"],
        )?,
        cache_write_unknown_tokens: optional_u64(
            value,
            &[
                "cache_write_unknown_tokens",
                "cache_creation_input_tokens",
                "cache_write_input_tokens",
            ],
        )?,
        output_tokens: optional_u64(value, &["output_tokens", "output_tokens_total"])?,
    })
}

fn fill_missing_counters(target: &mut ReconciliationCounters, fallback: &ReconciliationCounters) {
    if target.request_count.is_none() {
        target.request_count = fallback.request_count;
    }
    if target.input_tokens_uncached.is_none() {
        target.input_tokens_uncached = fallback.input_tokens_uncached;
    }
    if target.input_tokens_cached.is_none() {
        target.input_tokens_cached = fallback.input_tokens_cached;
    }
    if target.cache_write_5m_tokens.is_none() {
        target.cache_write_5m_tokens = fallback.cache_write_5m_tokens;
    }
    if target.cache_write_1h_tokens.is_none() {
        target.cache_write_1h_tokens = fallback.cache_write_1h_tokens;
    }
    if target.cache_write_unknown_tokens.is_none() {
        target.cache_write_unknown_tokens = fallback.cache_write_unknown_tokens;
    }
    if target.output_tokens.is_none() {
        target.output_tokens = fallback.output_tokens;
    }
}

fn routing_from_value(value: &Value) -> ReconciliationRouting {
    ReconciliationRouting {
        inference_geo: optional_string(value, &["inference_geo", "inference_geography"])
            .and_then(|text| safe_identifier(&text, 64)),
        service_tier: optional_string(value, &["service_tier"])
            .and_then(|text| safe_identifier(&text, 64)),
        provider_route: optional_string(value, &["provider_route", "route"])
            .and_then(|text| safe_identifier(&text, 64)),
    }
}

fn usd_from_value(value: &Value) -> Result<Option<Decimal>> {
    let candidate = value
        .get("provider_metered_usd")
        .or_else(|| value.get("usd"))
        .or_else(|| value.pointer("/amount/value"))
        .or_else(|| value.pointer("/cost/amount"))
        .or_else(|| value.get("amount").filter(|amount| !amount.is_object()))
        .or_else(|| value.get("cost").filter(|cost| !cost.is_object()));
    let currency = optional_string(value, &["currency"])
        .or_else(|| value.pointer("/amount/currency").and_then(value_string))
        .or_else(|| value.pointer("/cost/currency").and_then(value_string));
    if currency
        .as_deref()
        .is_some_and(|currency| !currency.eq_ignore_ascii_case("usd"))
    {
        return Ok(None);
    }
    candidate.map(parse_decimal).transpose()
}

fn is_cost_value(value: &Value) -> bool {
    value.get("amount").is_some()
        || value.get("cost").is_some()
        || value.get("provider_metered_usd").is_some()
}

fn required_datetime(value: &Value, keys: &[&str]) -> Result<DateTime<Utc>> {
    optional_datetime(value, keys)?.context("required timestamp is missing")
}

fn optional_datetime(value: &Value, keys: &[&str]) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = find_value(value, keys) else {
        return Ok(None);
    };
    if let Some(seconds) = value.as_i64() {
        return DateTime::from_timestamp(seconds, 0)
            .context("timestamp is outside the supported range")
            .map(Some);
    }
    let text = value
        .as_str()
        .context("timestamp must be RFC3339, YYYY-MM-DD, or Unix seconds")?;
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(text) {
        return Ok(Some(timestamp.with_timezone(&Utc)));
    }
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Ok(Some(DateTime::<Utc>::from_naive_utc_and_offset(
            date.and_hms_opt(0, 0, 0).context("invalid date")?,
            Utc,
        )));
    }
    anyhow::bail!("timestamp has an unsupported representation")
}

fn required_safe(
    value: &Value,
    keys: &[&str],
    max_len: usize,
    label: &str,
    ordinal: usize,
) -> Result<String> {
    optional_string(value, keys)
        .and_then(|text| safe_identifier(&text, max_len))
        .with_context(|| format!("canonical bucket {ordinal} requires a safe {label}"))
}

fn optional_string(value: &Value, keys: &[&str]) -> Option<String> {
    find_value(value, keys).and_then(value_string)
}

fn value_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn optional_u64(value: &Value, keys: &[&str]) -> Result<Option<u64>> {
    let Some(value) = find_value(value, keys) else {
        return Ok(None);
    };
    if let Some(number) = value.as_u64() {
        return Ok(Some(number));
    }
    if let Some(text) = value.as_str() {
        return text
            .trim()
            .parse::<u64>()
            .context("token and request counters must be non-negative integers")
            .map(Some);
    }
    anyhow::bail!("token and request counters must be non-negative integers")
}

fn find_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key))
}

fn parse_decimal(value: &Value) -> Result<Decimal> {
    let text = value_string(value).context("USD amount must be a string or number")?;
    let amount = Decimal::from_str(&text).context("USD amount has an invalid decimal value")?;
    if amount.is_sign_negative() {
        anyhow::bail!("USD amount cannot be negative");
    }
    Ok(amount)
}

fn safe_identifier(value: &str, max_len: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > max_len
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '.' | ':' | '/' | ' ')
        })
    {
        return None;
    }
    Some(value.to_string())
}

fn normalize_provider(value: &str) -> String {
    match value
        .trim()
        .to_ascii_lowercase()
        .replace(['-', '_'], "")
        .as_str()
    {
        "openai" => "openai".to_string(),
        "anthropic" | "claude" => "anthropic".to_string(),
        _ => safe_identifier(&value.to_ascii_lowercase(), 32)
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

fn normalize_buckets(buckets: &mut Vec<ReconciliationBucketDraft>) -> Result<()> {
    let mut normalized: BTreeMap<NormalizedBucketKey, ReconciliationBucketDraft> = BTreeMap::new();
    for mut bucket in buckets.drain(..) {
        if bucket.bucket_end <= bucket.bucket_start {
            anyhow::bail!("provider bucket end must be after its start");
        }
        if bucket.bucket_end - bucket.bucket_start > Duration::days(366) {
            anyhow::bail!("provider bucket exceeds the one-year safety limit");
        }
        if !bucket.counters.any_reported() && bucket.provider_metered_usd.is_none() {
            anyhow::bail!("provider bucket contains neither supported counters nor a USD amount");
        }
        bucket.provider = normalize_provider(&bucket.provider);
        if bucket.provider == "unknown" {
            anyhow::bail!("provider bucket has an unsupported provider identifier");
        }
        let key = (
            bucket.source_kind.clone(),
            bucket.bucket_start,
            bucket.bucket_end,
            bucket.provider.clone(),
            bucket.model.clone(),
            bucket.routing.inference_geo.clone(),
            bucket.routing.service_tier.clone(),
            bucket.routing.provider_route.clone(),
        );
        if let Some(existing) = normalized.get_mut(&key) {
            existing.counters.checked_add_assign(&bucket.counters)?;
            if let Some(amount) = bucket.provider_metered_usd {
                existing.provider_metered_usd = Some(
                    existing
                        .provider_metered_usd
                        .unwrap_or(Decimal::ZERO)
                        .checked_add(amount)
                        .context("reconciliation USD total exceeds the supported decimal range")?,
                );
            }
        } else {
            normalized.insert(key, bucket);
        }
    }
    *buckets = normalized.into_values().collect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    use crate::db::SourceUpdate;
    use crate::model::{
        CoverageStatus, PricingDimensions, UsageObservation, UsageQuality, UsageVector,
    };

    fn add_local_event(
        ledger: &mut Ledger,
        occurred_at: &str,
        provider: &str,
        model: &str,
        uncached: u64,
        cached: u64,
        output: u64,
    ) -> Result<()> {
        let dir = tempdir()?;
        let path = dir
            .path()
            .join(format!("{}.jsonl", occurred_at.replace(':', "-")));
        let client = if provider == "anthropic" {
            Client::ClaudeCode
        } else {
            Client::OpenaiCodex
        };
        let source_id = ledger.ensure_source(client, &path, false)?;
        let run = ledger.start_scan("reconcile-test")?;
        let observation = UsageObservation {
            event_key: format!("event-{occurred_at}-{model}"),
            client,
            session_id: "private-local-session".to_string(),
            usage_event_index: None,
            provider_message_id: None,
            occurred_at: DateTime::parse_from_rfc3339(occurred_at)?.with_timezone(&Utc),
            raw_model: model.to_string(),
            provider: provider.to_string(),
            usage: UsageVector {
                input_tokens_total: uncached + cached,
                input_tokens_uncached: uncached,
                input_tokens_cached: cached,
                output_tokens_total: output,
                ..Default::default()
            },
            dimensions: PricingDimensions::default(),
            quality: UsageQuality::Exact,
            coverage: CoverageStatus::CompleteKnown,
            source_locator: "line 1 @ byte 0".to_string(),
            parser_version: "reconcile-test".to_string(),
            warnings: Vec::new(),
        };
        let state = Value::Null;
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
            observations: &[observation],
            warnings: &[],
            scan_run_id: run,
        })?;
        ledger.finish_scan(run, 1, 1, 0, "ok")?;
        Ok(())
    }

    fn canonical_import(
        start: &str,
        end: &str,
        provider: &str,
        model: &str,
        uncached: u64,
        cached: u64,
        output: u64,
    ) -> Result<ParsedReconciliationImport> {
        let document = serde_json::json!({
            "schema_version": RECONCILIATION_SCHEMA_VERSION,
            "source_kind": "test_provider_usage",
            "buckets": [{
                "start": start,
                "end": end,
                "provider": provider,
                "model": model,
                "request_count": 1,
                "input_tokens_uncached": uncached,
                "input_tokens_cached": cached,
                "output_tokens": output
            }]
        });
        parse_import(&serde_json::to_vec(&document)?, ImportFormat::CanonicalJson)
    }

    #[test]
    fn openai_native_usage_derives_uncached_and_drops_ids() -> Result<()> {
        let raw = include_bytes!("../tests/fixtures/openai_organization_usage.json");
        let parsed = parse_import(raw, ImportFormat::Openai)?;
        assert_eq!(parsed.buckets.len(), 1);
        assert_eq!(parsed.buckets[0].counters.input_tokens_uncached, Some(100));
        assert_eq!(parsed.buckets[0].counters.input_tokens_cached, Some(20));
        let debug = format!("{parsed:?}");
        assert!(!debug.contains("project-private-canary"));
        assert!(!debug.contains("key-private-canary"));
        Ok(())
    }

    #[test]
    fn anthropic_native_usage_reads_cache_classes_and_routing() -> Result<()> {
        let raw = include_bytes!("../tests/fixtures/anthropic_admin_usage.json");
        let parsed = parse_import(raw, ImportFormat::Anthropic)?;
        let bucket = &parsed.buckets[0];
        assert_eq!(bucket.counters.cache_write_5m_tokens, Some(3));
        assert_eq!(bucket.counters.cache_write_1h_tokens, Some(4));
        assert_eq!(bucket.routing.inference_geo.as_deref(), Some("global"));
        assert!(!format!("{parsed:?}").contains("workspace-private-canary"));
        Ok(())
    }

    #[test]
    fn canonical_csv_preserves_missing_as_unknown() -> Result<()> {
        let raw = b"bucket_start,bucket_end,provider,model,input_tokens_uncached,output_tokens\n2026-07-10,2026-07-11,openai,gpt-5.6-sol,10,2\n";
        let parsed = parse_import(raw, ImportFormat::CanonicalCsv)?;
        assert_eq!(parsed.buckets[0].counters.input_tokens_cached, None);
        assert_eq!(parsed.buckets[0].counters.input_tokens_uncached, Some(10));
        Ok(())
    }

    #[test]
    fn native_cost_exports_retain_only_explicit_usd() -> Result<()> {
        let openai = br#"{"data":[{"start_time":1783641600,"end_time":1783728000,
            "results":[{"amount":{"value":1.25,"currency":"usd"},"project_id":"private"}]}]}"#;
        let openai = parse_import(openai, ImportFormat::Openai)?;
        assert_eq!(
            openai.buckets[0].provider_metered_usd,
            Some(Decimal::from_str("1.25")?)
        );
        assert_eq!(openai.buckets[0].source_kind, "openai_organization_costs");

        let anthropic = br#"{"data":[{"starting_at":"2026-07-10T00:00:00Z",
            "ending_at":"2026-07-11T00:00:00Z","amount":"2.50","currency":"USD",
            "workspace_id":"private"}]}"#;
        let anthropic = parse_import(anthropic, ImportFormat::Anthropic)?;
        assert_eq!(
            anthropic.buckets[0].provider_metered_usd,
            Some(Decimal::from_str("2.50")?)
        );
        assert_eq!(anthropic.buckets[0].source_kind, "anthropic_admin_costs");
        Ok(())
    }

    #[test]
    fn exact_content_digest_is_stable() -> Result<()> {
        let raw = br#"{"schema_version":"token-ledger.reconciliation.v1","buckets":[{"start":"2026-07-10","end":"2026-07-11","provider":"openai","model":"gpt-test","input_tokens_uncached":1}]}"#;
        let left = parse_import(raw, ImportFormat::CanonicalJson)?;
        let right = parse_import(raw, ImportFormat::CanonicalJson)?;
        assert_eq!(left.content_digest, right.content_digest);
        assert_eq!(left.content_digest.len(), 64);
        Ok(())
    }

    #[test]
    fn duplicate_extreme_usd_buckets_return_validation_error_instead_of_panicking() -> Result<()> {
        let maximum = Decimal::MAX.to_string();
        let document = serde_json::json!({
            "schema_version": RECONCILIATION_SCHEMA_VERSION,
            "source_kind": "extreme_cost_fixture",
            "buckets": [
                {"start":"2026-07-10","end":"2026-07-11","provider":"openai","provider_metered_usd":maximum},
                {"start":"2026-07-10","end":"2026-07-11","provider":"openai","provider_metered_usd":maximum}
            ]
        });
        let error = parse_import(&serde_json::to_vec(&document)?, ImportFormat::CanonicalJson)
            .expect_err("extreme USD sum must be rejected");
        assert!(error.to_string().contains("USD total exceeds"));
        Ok(())
    }

    #[test]
    fn duplicate_extreme_counter_buckets_reject_u64_overflow() -> Result<()> {
        let document = serde_json::json!({
            "schema_version": RECONCILIATION_SCHEMA_VERSION,
            "source_kind": "extreme_usage_fixture",
            "buckets": [
                {"start":"2026-07-10","end":"2026-07-11","provider":"openai","input_tokens_uncached":u64::MAX},
                {"start":"2026-07-10","end":"2026-07-11","provider":"openai","input_tokens_uncached":1}
            ]
        });
        let error = parse_import(&serde_json::to_vec(&document)?, ImportFormat::CanonicalJson)
            .expect_err("extreme counter sum must be rejected");
        assert!(error.to_string().contains("u64 accounting limit"));
        Ok(())
    }

    #[test]
    fn exact_reimport_is_idempotent_and_does_not_create_usage() -> Result<()> {
        let mut ledger = Ledger::open_in_memory()?;
        let parsed = canonical_import(
            "2026-07-10T00:00:00Z",
            "2026-07-11T00:00:00Z",
            "openai",
            "gpt-test",
            10,
            2,
            3,
        )?;
        assert!(ledger.store_reconciliation_import(&parsed)?.imported);
        assert!(!ledger.store_reconciliation_import(&parsed)?.imported);
        assert_eq!(ledger.reconciliation_imports()?.len(), 1);
        assert_eq!(ledger.reconciliation_buckets()?.len(), 1);
        assert_eq!(ledger.stats()?.observations, 0);
        Ok(())
    }

    #[test]
    fn explicit_provider_zero_matches_empty_local_bucket() -> Result<()> {
        let mut ledger = Ledger::open_in_memory()?;
        let mut parsed = canonical_import(
            "2026-07-10T00:00:00Z",
            "2026-07-11T00:00:00Z",
            "openai",
            "gpt-test",
            0,
            0,
            0,
        )?;
        parsed.buckets[0].counters.request_count = Some(0);
        ledger.store_reconciliation_import(&parsed)?;
        let compared = report(&ledger, None, None, chrono_tz::UTC)?;
        assert_eq!(compared.summary.matched, 1);
        assert_eq!(compared.summary.provider_only, 0);
        Ok(())
    }

    #[test]
    fn report_classifies_match_and_counter_mismatch() -> Result<()> {
        let mut ledger = Ledger::open_in_memory()?;
        add_local_event(
            &mut ledger,
            "2026-07-10T12:00:00Z",
            "openai",
            "gpt-test",
            10,
            2,
            3,
        )?;
        let matching = canonical_import(
            "2026-07-10T00:00:00Z",
            "2026-07-11T00:00:00Z",
            "openai",
            "gpt-test",
            10,
            2,
            3,
        )?;
        ledger.store_reconciliation_import(&matching)?;
        let matched = report(&ledger, None, None, chrono_tz::UTC)?;
        assert_eq!(matched.summary.matched, 1);

        let mut mismatch_document = serde_json::to_value(serde_json::json!({
            "schema_version": RECONCILIATION_SCHEMA_VERSION,
            "source_kind": "test_provider_usage",
            "buckets": [{
                "start": "2026-07-10T00:00:00Z", "end": "2026-07-11T00:00:00Z",
                "provider": "openai", "model": "gpt-test", "request_count": 1,
                "input_tokens_uncached": 11, "input_tokens_cached": 2, "output_tokens": 3
            }]
        }))?;
        mismatch_document["revision_nonce"] = Value::from("new-provider-snapshot");
        let mismatch = parse_import(
            &serde_json::to_vec(&mismatch_document)?,
            ImportFormat::CanonicalJson,
        )?;
        ledger.store_reconciliation_import(&mismatch)?;
        let compared = report(&ledger, None, None, chrono_tz::UTC)?;
        assert_eq!(compared.summary.counter_mismatch, 1);
        assert_eq!(
            compared.rows[0]
                .deltas_provider_minus_local
                .input_tokens_uncached,
            Some(1)
        );
        Ok(())
    }

    #[test]
    fn exclusive_end_activity_is_flagged_as_time_boundary() -> Result<()> {
        let mut ledger = Ledger::open_in_memory()?;
        add_local_event(
            &mut ledger,
            "2026-07-10T01:00:00Z",
            "openai",
            "gpt-test",
            10,
            0,
            1,
        )?;
        let parsed = canonical_import(
            "2026-07-10T00:00:00Z",
            "2026-07-10T01:00:00Z",
            "openai",
            "gpt-test",
            10,
            0,
            1,
        )?;
        ledger.store_reconciliation_import(&parsed)?;
        let compared = report(&ledger, None, None, chrono_tz::UTC)?;
        assert!(
            compared
                .rows
                .iter()
                .any(|row| { row.classification == ReconciliationClassification::TimeBoundary })
        );
        Ok(())
    }

    #[test]
    fn local_only_day_uses_dst_aware_boundaries() -> Result<()> {
        let mut ledger = Ledger::open_in_memory()?;
        add_local_event(
            &mut ledger,
            "2026-03-08T12:00:00Z",
            "openai",
            "gpt-test",
            1,
            0,
            1,
        )?;
        let compared = report(&ledger, None, None, chrono_tz::America::New_York)?;
        let row = compared
            .rows
            .iter()
            .find(|row| row.classification == ReconciliationClassification::LocalOnly)
            .expect("local-only row");
        assert_eq!(
            row.bucket_end_utc - row.bucket_start_utc,
            Duration::hours(23)
        );
        Ok(())
    }
}
