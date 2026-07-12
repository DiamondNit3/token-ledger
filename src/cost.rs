//! First-class cost reporting.
//!
//! Cost reports deliberately keep three concepts separate:
//! API list-price equivalents, provider-specific units, and user-attested
//! cash billing. None of those values is silently relabelled as another.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::billing::{
    ActualBillingStatus, BillingEvidence, BillingQuery, BillingWindow, ProviderBillingTotal,
};
use crate::model::{
    CanonicalEvent, Client, CoverageWindowStatus, LedgerCoverageSnapshot, UsageVector,
};
use crate::pricing::{
    CatalogSource, CatalogStatus, MeasureStatus, MissingPriceComponent, PricingAssumptionEvidence,
    PricingEngine, PricingMeasureEstimate,
};
use crate::reconcile::{ReconciliationClassification, ReconciliationReport, ReconciliationSummary};
use crate::report::{QueryCoverageStatus, canonical_model_name, local_range_bounds};
use crate::terminal::{
    Layout, TerminalOptions, Tone, display_client_name, display_model_name, format_count_compact,
    format_decimal, format_percent,
};

pub const COST_SCHEMA_VERSION: &str = "token-ledger.cost.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostPeriodKind {
    Today,
    Yesterday,
    CurrentMonth,
    AllLocalHistory,
    ExplicitRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostPeriodSelection {
    Today,
    Yesterday,
    CurrentMonth,
    AllLocalHistory,
    ExplicitRange { start: NaiveDate, end: NaiveDate },
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedCostPeriod {
    pub kind: CostPeriodKind,
    pub requested_start_date: Option<String>,
    pub requested_end_date_inclusive: Option<String>,
    pub start_utc: Option<DateTime<Utc>>,
    pub end_utc_exclusive: Option<DateTime<Utc>>,
}

/// Resolve the selected local-calendar period. For `all`, bounds describe the
/// actual selected local event history. An empty history remains unbounded
/// rather than inventing a zero-length or current-day interval.
pub fn resolve_cost_period(
    selection: CostPeriodSelection,
    timezone: Tz,
    now: DateTime<Utc>,
    all_history_events: &[CanonicalEvent],
) -> Result<ResolvedCostPeriod> {
    let today = now.with_timezone(&timezone).date_naive();
    let (kind, dates) = match selection {
        CostPeriodSelection::Today => (CostPeriodKind::Today, Some((today, today))),
        CostPeriodSelection::Yesterday => {
            let yesterday = today
                .checked_sub_signed(Duration::days(1))
                .context("yesterday is outside the supported date range")?;
            (CostPeriodKind::Yesterday, Some((yesterday, yesterday)))
        }
        CostPeriodSelection::CurrentMonth => {
            let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                .context("current month is outside the supported date range")?;
            (CostPeriodKind::CurrentMonth, Some((first, today)))
        }
        CostPeriodSelection::ExplicitRange { start, end } => {
            anyhow::ensure!(end >= start, "cost range end must be on or after start");
            (CostPeriodKind::ExplicitRange, Some((start, end)))
        }
        CostPeriodSelection::AllLocalHistory => {
            let dates = all_history_events
                .iter()
                .map(|event| event.occurred_at.with_timezone(&timezone).date_naive())
                .fold(None, |bounds, date| match bounds {
                    None => Some((date, date)),
                    Some((start, end)) => Some((start.min(date), end.max(date))),
                });
            (CostPeriodKind::AllLocalHistory, dates)
        }
    };
    let (start_utc, end_utc_exclusive) = dates
        .map(|(start, end)| local_range_bounds(start, end, timezone))
        .transpose()?
        .map_or((None, None), |(start, end)| (Some(start), Some(end)));
    Ok(ResolvedCostPeriod {
        kind,
        requested_start_date: dates.map(|(start, _)| start.to_string()),
        requested_end_date_inclusive: dates.map(|(_, end)| end.to_string()),
        start_utc,
        end_utc_exclusive,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct CostQuery {
    pub period: ResolvedCostPeriod,
    pub timezone: String,
    pub client_filters: Vec<String>,
    pub model_filters: Vec<String>,
    pub scope_note: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CostUsage {
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

impl CostUsage {
    pub fn cache_write_tokens(&self) -> u64 {
        self.cache_write_5m_tokens
            .saturating_add(self.cache_write_1h_tokens)
            .saturating_add(self.cache_write_unknown_tokens)
    }

    fn add(&mut self, usage: &UsageVector) {
        self.input_tokens_total = self
            .input_tokens_total
            .saturating_add(usage.input_tokens_total);
        self.input_tokens_uncached = self
            .input_tokens_uncached
            .saturating_add(usage.input_tokens_uncached);
        self.input_tokens_cached = self
            .input_tokens_cached
            .saturating_add(usage.input_tokens_cached);
        self.cache_write_5m_tokens = self
            .cache_write_5m_tokens
            .saturating_add(usage.cache_write_5m_tokens);
        self.cache_write_1h_tokens = self
            .cache_write_1h_tokens
            .saturating_add(usage.cache_write_1h_tokens);
        self.cache_write_unknown_tokens = self
            .cache_write_unknown_tokens
            .saturating_add(usage.cache_write_unknown_tokens);
        self.output_tokens_total = self
            .output_tokens_total
            .saturating_add(usage.output_tokens_total);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens);
        self.web_search_requests = self
            .web_search_requests
            .saturating_add(usage.web_search_requests);
        self.web_fetch_requests = self
            .web_fetch_requests
            .saturating_add(usage.web_fetch_requests);
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CostMeasureEvidence {
    pub exact_events: u64,
    pub bounded_events: u64,
    pub partial_events: u64,
    pub unpriced_events: u64,
    pub unavailable_events: u64,
    pub dimension_evidence: Vec<PricingAssumptionEvidence>,
    pub missing_components: Vec<MissingPriceComponent>,
    /// Number of event-level explanation entries considered by this aggregate.
    pub explanation_count: u64,
    /// Number of event-level entries compacted into categories or omitted by
    /// the aggregate evidence limit. Use `token-ledger explain` for per-event math.
    pub omitted_explanation_count: u64,
    pub explanations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostMeasure {
    pub unit_name: String,
    pub status: MeasureStatus,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub lower_bound: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub upper_bound: Option<Decimal>,
    pub evidence: CostMeasureEvidence,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostModelRow {
    pub client: String,
    pub provider: String,
    pub model: String,
    pub requests: u64,
    pub sessions: u64,
    pub usage: CostUsage,
    pub api_equivalent_usd: CostMeasure,
    /// Kept as a list so unlike provider units are never arithmetically merged.
    pub provider_units: Vec<CostMeasure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostCombinedTotal {
    pub requests: u64,
    pub sessions: u64,
    pub usage: CostUsage,
    pub api_equivalent_usd: CostMeasure,
    /// One independent aggregate per unit name.
    pub provider_units: Vec<CostMeasure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostCatalogEvidence {
    pub status: CatalogStatus,
    pub sources: Vec<CatalogSource>,
    pub coverage_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostClientCoverage {
    pub client: Client,
    pub status: QueryCoverageStatus,
    pub matching_event_count: u64,
    pub source_count: u64,
    pub observation_count: u64,
    pub canonical_event_count: u64,
    pub earliest_observed_event_utc: Option<DateTime<Utc>>,
    pub latest_observed_event_utc: Option<DateTime<Utc>>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostCoverage {
    pub as_of: Option<DateTime<Utc>>,
    pub provisional: bool,
    pub active_or_volatile_source_count: u64,
    pub clients: Vec<CostClientCoverage>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostBilling {
    pub window: Option<BillingWindow>,
    pub provider_scope: BTreeSet<String>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub recorded_cash_usd: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub actual_billed_usd: Option<Decimal>,
    pub actual_billing_status: Option<ActualBillingStatus>,
    pub by_provider: BTreeMap<String, ProviderBillingTotal>,
    pub note: String,
}

impl CostBilling {
    pub fn unavailable(note: impl Into<String>) -> Self {
        Self {
            window: None,
            provider_scope: BTreeSet::new(),
            recorded_cash_usd: None,
            actual_billed_usd: None,
            actual_billing_status: None,
            by_provider: BTreeMap::new(),
            note: note.into(),
        }
    }

    pub fn from_evidence(
        evidence: &BillingEvidence,
        window: BillingWindow,
        providers: &BTreeSet<String>,
    ) -> Result<Self> {
        let query = BillingQuery::for_providers(window, providers)?;
        let aggregate = evidence.aggregate(&query)?;
        let actual_billed_usd = aggregate.attested_actual_billed_usd();
        Ok(Self {
            window: Some(aggregate.window),
            provider_scope: aggregate.provider_scope,
            recorded_cash_usd: Some(aggregate.recorded_cash_usd),
            // The billing module only populates this after complete bounded
            // attestations cover every provider in the closed scope.
            actual_billed_usd,
            actual_billing_status: Some(aggregate.actual_billing_status),
            by_provider: aggregate.by_provider,
            note: "Recorded cash is user-attested evidence. Actual billed USD is exposed only for an attested-complete provider scope and is never inferred from token prices. Model filters cannot allocate subscription or account-level cash movements.".to_string(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CostReconciliation {
    pub provider_evidence_present: bool,
    pub import_count: u64,
    pub selected_provider_bucket_count: u64,
    pub overlapping_evidence_row_count: u64,
    pub summary: ReconciliationSummary,
    pub limitations: Vec<String>,
    pub note: String,
}

impl CostReconciliation {
    pub fn unavailable(note: impl Into<String>) -> Self {
        Self {
            provider_evidence_present: false,
            import_count: 0,
            selected_provider_bucket_count: 0,
            overlapping_evidence_row_count: 0,
            summary: ReconciliationSummary::default(),
            limitations: Vec::new(),
            note: note.into(),
        }
    }

    pub fn from_report(
        report: &ReconciliationReport,
        providers: &BTreeSet<String>,
        model_filters: &[String],
        selected_models: &BTreeSet<String>,
    ) -> Self {
        let mut summary = ReconciliationSummary::default();
        let mut selected_provider_bucket_count = 0_u64;
        let mut overlapping_evidence_row_count = 0_u64;
        for row in &report.rows {
            if !providers.is_empty() && !providers.contains(&row.provider) {
                continue;
            }
            if !model_filters.is_empty()
                && row.model.as_ref().is_some_and(|model| {
                    !model_filters
                        .iter()
                        .any(|value| value.eq_ignore_ascii_case(model))
                        && !selected_models
                            .iter()
                            .any(|value| value.eq_ignore_ascii_case(model))
                })
            {
                continue;
            }
            record_reconciliation(&mut summary, row.classification);
            if row.provider_evidence.is_some() {
                selected_provider_bucket_count += 1;
                if row.local.is_some() {
                    overlapping_evidence_row_count += 1;
                }
            }
        }
        let provider_evidence_present = selected_provider_bucket_count > 0;
        Self {
            provider_evidence_present,
            import_count: report.import_count,
            selected_provider_bucket_count,
            overlapping_evidence_row_count,
            summary,
            limitations: report.limitations.clone(),
            note: if provider_evidence_present {
                "Provider evidence is compared independently and never overwrites local usage or API-equivalent estimates. Cash amounts from reconciliation exports are not promoted to actual billed USD.".to_string()
            } else {
                "No imported provider evidence overlapped the selected scope. Local totals remain local evidence, not provider-confirmed usage.".to_string()
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CostDocument {
    pub schema_version: String,
    pub generated_at_utc: DateTime<Utc>,
    pub query: CostQuery,
    pub catalog: CostCatalogEvidence,
    pub coverage: CostCoverage,
    pub models: Vec<CostModelRow>,
    pub combined: CostCombinedTotal,
    pub billing: CostBilling,
    pub reconciliation: CostReconciliation,
    pub interpretation: Vec<String>,
}

#[derive(Default)]
struct CostAccumulator {
    requests: u64,
    sessions: HashSet<(Client, String)>,
    usage: CostUsage,
    api_usd: MeasureAccumulator,
    provider_units: BTreeMap<String, MeasureAccumulator>,
}

impl CostAccumulator {
    fn add(&mut self, event: &CanonicalEvent, pricing: &PricingEngine) -> Result<()> {
        self.requests = self.requests.saturating_add(1);
        self.sessions
            .insert((event.client, event.session_id.clone()));
        self.usage.add(&event.usage);
        let estimate = pricing.estimate_event(event);
        self.api_usd.add(&estimate.api_equivalent_usd_measure)?;
        if event.client == Client::OpenaiCodex {
            self.provider_units
                .entry(estimate.provider_unit_measure.unit_name.clone())
                .or_default()
                .add(&estimate.provider_unit_measure)?;
        }
        Ok(())
    }

    fn finish(self) -> CostCombinedTotal {
        CostCombinedTotal {
            requests: self.requests,
            sessions: self.sessions.len() as u64,
            usage: self.usage,
            api_equivalent_usd: self.api_usd.finish("USD"),
            provider_units: finish_provider_units(self.provider_units),
        }
    }
}

#[derive(Default)]
struct MeasureAccumulator {
    exact_events: u64,
    bounded_events: u64,
    partial_events: u64,
    unpriced_events: u64,
    unavailable_events: u64,
    lower_bound: Decimal,
    upper_bound: Decimal,
    events_with_lower_bound: u64,
    events_with_upper_bound: u64,
    dimensions: BTreeMap<String, PricingAssumptionEvidence>,
    missing: BTreeMap<String, MissingPriceComponent>,
    explanations: BTreeSet<String>,
    explanation_count: u64,
}

const MAX_AGGREGATE_EXPLANATIONS: usize = 32;

fn aggregate_explanation(value: &str) -> String {
    const COMPONENTS: [&str; 8] = [
        "input_uncached",
        "cache_read",
        "cache_write_unknown",
        "cache_write_5m",
        "cache_write_1h",
        "output",
        "web_search_requests",
        "web_fetch_requests",
    ];

    if COMPONENTS
        .iter()
        .any(|component| value.starts_with(&format!("{component}:")))
    {
        let component = value.split_once(':').map_or("token", |(name, _)| name);
        return format!(
            "Per-event {component} arithmetic was aggregated into this measure's bounds."
        );
    }
    if value
        .starts_with("Unreported OpenAI cache writes are bounded by the inclusive input counter:")
    {
        return "Some OpenAI events did not report cache-write tokens; their cache writes were bounded by each event's inclusive input counter."
            .to_string();
    }
    value.to_string()
}

impl MeasureAccumulator {
    fn add(&mut self, measure: &PricingMeasureEstimate) -> Result<()> {
        match measure.status {
            MeasureStatus::Exact => self.exact_events += 1,
            MeasureStatus::Bounded => self.bounded_events += 1,
            MeasureStatus::Partial => self.partial_events += 1,
            MeasureStatus::Unpriced => self.unpriced_events += 1,
            MeasureStatus::Unavailable => self.unavailable_events += 1,
        }
        if let Some(lower) = measure.lower_bound {
            self.lower_bound = self
                .lower_bound
                .checked_add(lower)
                .context("API-equivalent lower-bound arithmetic overflowed")?;
            self.events_with_lower_bound += 1;
        }
        if let Some(upper) = measure.upper_bound {
            self.upper_bound = self
                .upper_bound
                .checked_add(upper)
                .context("API-equivalent upper-bound arithmetic overflowed")?;
            self.events_with_upper_bound += 1;
        }
        for evidence in &measure.dimension_evidence {
            let key = serde_json::to_string(evidence)?;
            self.dimensions
                .entry(key)
                .or_insert_with(|| evidence.clone());
        }
        for missing in &measure.missing_components {
            let key = format!(
                "{:?}|{}|{}",
                missing.rate_kind, missing.component, missing.reason
            );
            self.missing
                .entry(key)
                .and_modify(|aggregate| {
                    aggregate.quantity = match (aggregate.quantity, missing.quantity) {
                        (Some(left), Some(right)) => Some(left.saturating_add(right)),
                        _ => None,
                    };
                })
                .or_insert_with(|| missing.clone());
        }
        for explanation in &measure.explanation {
            self.explanation_count = self.explanation_count.saturating_add(1);
            let explanation = aggregate_explanation(explanation);
            if self.explanations.contains(&explanation)
                || self.explanations.len() < MAX_AGGREGATE_EXPLANATIONS
            {
                self.explanations.insert(explanation);
            }
        }
        Ok(())
    }

    fn applicable_events(&self) -> u64 {
        self.exact_events
            .saturating_add(self.bounded_events)
            .saturating_add(self.partial_events)
            .saturating_add(self.unpriced_events)
    }

    fn status(&self) -> MeasureStatus {
        let applicable = self.applicable_events();
        if applicable == 0 {
            MeasureStatus::Unavailable
        } else if self.exact_events == applicable && self.events_with_upper_bound == applicable {
            MeasureStatus::Exact
        } else if self.exact_events.saturating_add(self.bounded_events) == applicable
            && self.events_with_lower_bound == applicable
            && self.events_with_upper_bound == applicable
        {
            MeasureStatus::Bounded
        } else if self.unpriced_events == applicable {
            MeasureStatus::Unpriced
        } else {
            MeasureStatus::Partial
        }
    }

    fn finish(self, unit_name: &str) -> CostMeasure {
        let applicable = self.applicable_events();
        let status = self.status();
        let explanation_count = self.explanation_count;
        let explanations: Vec<_> = self.explanations.into_iter().collect();
        CostMeasure {
            unit_name: unit_name.to_string(),
            status,
            lower_bound: (self.events_with_lower_bound > 0).then_some(self.lower_bound),
            upper_bound: (applicable > 0 && self.events_with_upper_bound == applicable)
                .then_some(self.upper_bound),
            evidence: CostMeasureEvidence {
                exact_events: self.exact_events,
                bounded_events: self.bounded_events,
                partial_events: self.partial_events,
                unpriced_events: self.unpriced_events,
                unavailable_events: self.unavailable_events,
                dimension_evidence: self.dimensions.into_values().collect(),
                missing_components: self.missing.into_values().collect(),
                explanation_count,
                omitted_explanation_count: explanation_count
                    .saturating_sub(explanations.len() as u64),
                explanations,
            },
        }
    }
}

fn finish_provider_units(values: BTreeMap<String, MeasureAccumulator>) -> Vec<CostMeasure> {
    values
        .into_iter()
        .map(|(unit, aggregate)| aggregate.finish(&unit))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn build_cost_document(
    events: &[CanonicalEvent],
    query: CostQuery,
    pricing: &PricingEngine,
    coverage: LedgerCoverageSnapshot,
    billing: CostBilling,
    reconciliation: CostReconciliation,
) -> Result<CostDocument> {
    let mut groups: BTreeMap<(String, String, String), CostAccumulator> = BTreeMap::new();
    let mut combined = CostAccumulator::default();
    for event in events {
        let client = display_client(event.client).to_string();
        let model = canonical_model_name(pricing, event);
        groups
            .entry((client, event.provider.clone(), model))
            .or_default()
            .add(event, pricing)?;
        combined.add(event, pricing)?;
    }
    let models = groups
        .into_iter()
        .map(|((client, provider, model), aggregate)| {
            let total = aggregate.finish();
            CostModelRow {
                client,
                provider,
                model,
                requests: total.requests,
                sessions: total.sessions,
                usage: total.usage,
                api_equivalent_usd: total.api_equivalent_usd,
                provider_units: total.provider_units,
            }
        })
        .collect();
    let coverage = build_cost_coverage(events, &query.period, coverage);
    Ok(CostDocument {
        schema_version: COST_SCHEMA_VERSION.to_string(),
        generated_at_utc: Utc::now(),
        query,
        catalog: CostCatalogEvidence {
            status: pricing.status(),
            sources: pricing.catalog().sources().to_vec(),
            coverage_note: pricing.catalog().coverage_note().map(str::to_string),
        },
        coverage,
        models,
        combined: combined.finish(),
        billing,
        reconciliation,
        interpretation: vec![
            "API-equivalent USD is a public catalog/list-price estimate, not money paid and not a provider invoice.".to_string(),
            "Provider units are reported independently by unit name; unlike units are never added together.".to_string(),
            "Cached input is included in usage totals and may be priced differently from uncached input.".to_string(),
            "Local persistence can omit deleted, remote, expired, ephemeral, or other-machine sessions.".to_string(),
        ],
    })
}

fn build_cost_coverage(
    events: &[CanonicalEvent],
    period: &ResolvedCostPeriod,
    coverage: LedgerCoverageSnapshot,
) -> CostCoverage {
    let clients = coverage
        .clients
        .iter()
        .map(|snapshot| {
            let matching_event_count = events
                .iter()
                .filter(|event| event.client == snapshot.client)
                .count() as u64;
            let (status, note) = match snapshot.window_status {
                CoverageWindowStatus::NoSources => (
                    QueryCoverageStatus::NoSources,
                    "No persisted local source files were discovered for this client.".to_string(),
                ),
                CoverageWindowStatus::NoObservations => (
                    QueryCoverageStatus::NoObservations,
                    "Sources were indexed, but no supported usage observations are available."
                        .to_string(),
                ),
                CoverageWindowStatus::ObservedWindow if matching_event_count > 0 => (
                    QueryCoverageStatus::MatchingEvents,
                    "Persisted local events matched the selected scope.".to_string(),
                ),
                CoverageWindowStatus::ObservedWindow => {
                    let outside = period
                        .start_utc
                        .zip(period.end_utc_exclusive)
                        .is_some_and(|(start, end)| {
                            snapshot
                                .earliest_canonical_event
                                .as_ref()
                                .zip(snapshot.latest_canonical_event.as_ref())
                                .is_some_and(|(earliest, latest)| {
                                    end <= earliest.occurred_at || start > latest.occurred_at
                                })
                        });
                    if outside {
                        (
                            QueryCoverageStatus::OutsideObservedWindow,
                            "The selected interval is outside this ledger's observed local event window."
                                .to_string(),
                        )
                    } else {
                        (
                            QueryCoverageStatus::NoEventsWithinObservedWindow,
                            "No persisted events matched this scope; this is not proof of zero provider usage."
                                .to_string(),
                        )
                    }
                }
            };
            CostClientCoverage {
                client: snapshot.client,
                status,
                matching_event_count,
                source_count: snapshot.source_count,
                observation_count: snapshot.observation_count,
                canonical_event_count: snapshot.canonical_event_count,
                earliest_observed_event_utc: snapshot
                    .earliest_canonical_event
                    .as_ref()
                    .map(|value| value.occurred_at),
                latest_observed_event_utc: snapshot
                    .latest_canonical_event
                    .as_ref()
                    .map(|value| value.occurred_at),
                note,
            }
        })
        .collect();
    CostCoverage {
        as_of: coverage.as_of,
        provisional: coverage.provisional,
        active_or_volatile_source_count: coverage.active_or_volatile_source_count,
        clients,
        note: "Coverage describes readable local persistence, not a provider invoice or proof that no other usage occurred.".to_string(),
    }
}

fn record_reconciliation(
    summary: &mut ReconciliationSummary,
    classification: ReconciliationClassification,
) {
    match classification {
        ReconciliationClassification::Matched => summary.matched += 1,
        ReconciliationClassification::LocalOnly => summary.local_only += 1,
        ReconciliationClassification::ProviderOnly => summary.provider_only += 1,
        ReconciliationClassification::CounterMismatch => summary.counter_mismatch += 1,
        ReconciliationClassification::TimeBoundary => summary.time_boundary += 1,
        ReconciliationClassification::RouteUnknown => summary.route_unknown += 1,
    }
}

pub fn render_cost(document: &CostDocument) -> String {
    render_cost_with_options(document, &TerminalOptions::default())
}

pub fn render_cost_with_options(document: &CostDocument, terminal: &TerminalOptions) -> String {
    let mut output = String::new();
    let start = document
        .query
        .period
        .requested_start_date
        .as_deref()
        .unwrap_or("first matching local event");
    let end = document
        .query
        .period
        .requested_end_date_inclusive
        .as_deref()
        .unwrap_or("last matching local event");
    let sep = terminal.separator();
    let catalog_tone = if document.catalog.status.verification.error_count() == 0 {
        Tone::Success
    } else {
        Tone::Error
    };
    let snapshot_tone = if document.coverage.provisional {
        Tone::Warning
    } else {
        Tone::Success
    };
    let snapshot_label = if document.coverage.provisional {
        "PROVISIONAL"
    } else {
        "STABLE"
    };
    let total_tokens = document
        .combined
        .usage
        .input_tokens_total
        .saturating_add(document.combined.usage.output_tokens_total);

    let _ = writeln!(
        output,
        "{}",
        terminal.paint(Tone::Accent, "TOKEN LEDGER / COST")
    );
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(
            Tone::Muted,
            format!("{start} through {end}{sep}{}", document.query.timezone)
        )
    );
    if terminal.layout() == Layout::Narrow {
        let _ = writeln!(
            output,
            "Catalog {} {}",
            document.catalog.status.revision,
            terminal.badge("VERIFIED", catalog_tone)
        );
        let _ = writeln!(
            output,
            "Snapshot {}",
            terminal.badge(snapshot_label, snapshot_tone)
        );
    } else {
        let _ = writeln!(
            output,
            "Catalog {} {}{sep}Snapshot {}",
            document.catalog.status.revision,
            terminal.badge("VERIFIED", catalog_tone),
            terminal.badge(snapshot_label, snapshot_tone),
        );
    }
    output.push('\n');

    let combined_measure =
        format_measure_human(&document.combined.api_equivalent_usd, terminal, true);
    let (combined_status, combined_tone) = measure_badge(&document.combined.api_equivalent_usd);
    let _ = writeln!(
        output,
        "{}  {}",
        terminal.paint(Tone::Strong, combined_measure),
        terminal.badge(combined_status, combined_tone)
    );
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(
            Tone::Muted,
            "API-equivalent list-price value (not money paid)"
        )
    );
    output.push('\n');

    match terminal.layout() {
        Layout::Wide => {
            let _ = writeln!(
                output,
                "{} tokens{sep}{} requests{sep}{} sessions{sep}{} cached",
                terminal.paint(Tone::Strong, format_count_compact(total_tokens)),
                terminal.paint(Tone::Strong, format_count(document.combined.requests)),
                terminal.paint(Tone::Strong, format_count(document.combined.sessions)),
                terminal.paint(
                    Tone::Strong,
                    format_percent(
                        document.combined.usage.input_tokens_cached,
                        document.combined.usage.input_tokens_total
                    )
                )
            );
        }
        Layout::Compact | Layout::Narrow => {
            let _ = writeln!(output, "Tokens:   {}", format_count(total_tokens));
            let _ = writeln!(
                output,
                "Requests: {}{sep}Sessions: {}",
                format_count(document.combined.requests),
                format_count(document.combined.sessions)
            );
        }
    }
    output.push('\n');

    render_cost_models(document, terminal, &mut output);
    render_provider_units(document, terminal, &mut output);

    let recorded = document
        .billing
        .recorded_cash_usd
        .map(|value| format_decimal(value, true))
        .unwrap_or_else(|| "Unavailable".to_string());
    let actual = document
        .billing
        .actual_billed_usd
        .map(|value| format_decimal(value, true))
        .unwrap_or_else(|| "Unknown".to_string());
    let billing_complete = document.billing.actual_billed_usd.is_some();
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(Tone::Accent, "BILLING EVIDENCE")
    );
    let recorded_badge = terminal.badge(
        if billing_complete {
            "ATTESTED COMPLETE"
        } else {
            "INCOMPLETE EVIDENCE"
        },
        if billing_complete {
            Tone::Success
        } else {
            Tone::Warning
        },
    );
    let actual_badge = terminal.badge(
        if billing_complete {
            "ATTESTED"
        } else {
            "NOT ATTESTED"
        },
        if billing_complete {
            Tone::Success
        } else {
            Tone::Warning
        },
    );
    if terminal.layout() == Layout::Narrow {
        let _ = writeln!(output, "Recorded cash: {recorded}");
        let _ = writeln!(output, "  {recorded_badge}");
        let _ = writeln!(output, "Actual billed: {actual}");
        let _ = writeln!(output, "  {actual_badge}");
    } else {
        let _ = writeln!(output, "Recorded cash   {recorded:<16} {recorded_badge}");
        let _ = writeln!(output, "Actual billed   {actual:<16} {actual_badge}");
    }
    output.push('\n');

    let snapshot_at = document
        .coverage
        .as_of
        .map(|value| value.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "as-of unavailable".to_string());
    if terminal.layout() == Layout::Narrow {
        let _ = writeln!(
            output,
            "{} Snapshot {}",
            terminal.paint(snapshot_tone, terminal.status_symbol(snapshot_tone)),
            snapshot_label.to_ascii_lowercase()
        );
        let _ = writeln!(output, "  As of: {snapshot_at}");
        let _ = writeln!(
            output,
            "  Active or volatile sources: {}",
            format_count(document.coverage.active_or_volatile_source_count)
        );
    } else {
        let _ = writeln!(
            output,
            "{} Snapshot {}{sep}{} active or volatile sources",
            terminal.paint(snapshot_tone, terminal.status_symbol(snapshot_tone)),
            snapshot_at,
            format_count(document.coverage.active_or_volatile_source_count),
        );
    }
    if document.reconciliation.provider_evidence_present {
        let _ = writeln!(
            output,
            "{} Reconciliation compared {} provider bucket(s); local totals were unchanged.",
            terminal.paint(Tone::Accent, terminal.status_symbol(Tone::Accent)),
            format_count(document.reconciliation.selected_provider_bucket_count)
        );
    }
    if terminal.details {
        output.push('\n');
        let _ = writeln!(output, "{}", terminal.paint(Tone::Accent, "DETAILS"));
        let _ = writeln!(
            output,
            "Catalog SHA-256: {}",
            document.catalog.status.sha256
        );
        let _ = writeln!(output, "Billing: {}", document.billing.note);
        let _ = writeln!(output, "Reconciliation: {}", document.reconciliation.note);
        for client in &document.coverage.clients {
            let _ = writeln!(
                output,
                "Coverage {}: {:?}{}{} matching event(s)",
                display_client_name(client.client.as_str()),
                client.status,
                sep,
                format_count(client.matching_event_count)
            );
        }
    } else if !terminal.plain {
        let _ = writeln!(
            output,
            "{}",
            terminal.paint(Tone::Muted, "Details: rerun with --details")
        );
    }
    output
}

fn render_cost_models(document: &CostDocument, terminal: &TerminalOptions, output: &mut String) {
    let _ = writeln!(output, "{}", terminal.paint(Tone::Accent, "BY MODEL"));
    if document.models.is_empty() {
        let _ = writeln!(
            output,
            "No matching local events. This is not a verified zero."
        );
        output.push('\n');
        return;
    }
    match terminal.layout() {
        Layout::Wide => {
            let _ = writeln!(
                output,
                "{:<22} {:<13} {:>9} {:>12} {:>24}  STATUS",
                "MODEL", "CLIENT", "REQUESTS", "TOKENS", "ESTIMATE"
            );
            let _ = writeln!(output, "{}", terminal.rule(100));
            for row in &document.models {
                let tokens = row
                    .usage
                    .input_tokens_total
                    .saturating_add(row.usage.output_tokens_total);
                let estimate = format_measure_human(&row.api_equivalent_usd, terminal, true);
                let (status, tone) = measure_badge(&row.api_equivalent_usd);
                let _ = writeln!(
                    output,
                    "{:<22} {:<13} {:>9} {:>12} {:>24}  {}",
                    truncate(&display_model_name(&row.model), 22),
                    truncate(&display_client_name(&row.client), 13),
                    format_count(row.requests),
                    format_count_compact(tokens),
                    estimate,
                    terminal.badge(status, tone)
                );
            }
        }
        Layout::Compact => {
            for row in &document.models {
                let tokens = row
                    .usage
                    .input_tokens_total
                    .saturating_add(row.usage.output_tokens_total);
                let (status, tone) = measure_badge(&row.api_equivalent_usd);
                let _ = writeln!(
                    output,
                    "{}{}{}  {}",
                    terminal.paint(Tone::Strong, display_model_name(&row.model)),
                    terminal.separator(),
                    display_client_name(&row.client),
                    terminal.badge(status, tone)
                );
                let _ = writeln!(
                    output,
                    "  {} requests{}{} tokens{}{}",
                    format_count(row.requests),
                    terminal.separator(),
                    format_count_compact(tokens),
                    terminal.separator(),
                    format_measure_human(&row.api_equivalent_usd, terminal, true)
                );
            }
        }
        Layout::Narrow => {
            for row in &document.models {
                let tokens = row
                    .usage
                    .input_tokens_total
                    .saturating_add(row.usage.output_tokens_total);
                let (status, tone) = measure_badge(&row.api_equivalent_usd);
                let _ = writeln!(output, "{}", display_model_name(&row.model));
                let _ = writeln!(output, "  Client:   {}", display_client_name(&row.client));
                let _ = writeln!(output, "  Requests: {}", format_count(row.requests));
                let _ = writeln!(output, "  Tokens:   {}", format_count(tokens));
                let _ = writeln!(
                    output,
                    "  Estimate: {} {}",
                    format_measure_human(&row.api_equivalent_usd, terminal, true),
                    terminal.badge(status, tone)
                );
            }
        }
    }
    output.push('\n');
}

fn render_provider_units(document: &CostDocument, terminal: &TerminalOptions, output: &mut String) {
    if document
        .models
        .iter()
        .all(|row| row.provider_units.is_empty())
    {
        return;
    }
    let _ = writeln!(output, "{}", terminal.paint(Tone::Accent, "PROVIDER UNITS"));
    for row in &document.models {
        for units in &row.provider_units {
            let (status, tone) = measure_badge(units);
            if terminal.layout() == Layout::Narrow {
                let _ = writeln!(output, "{}", display_model_name(&row.model));
                let _ = writeln!(
                    output,
                    "  {} {}",
                    format_measure_human(units, terminal, false),
                    units.unit_name
                );
                let _ = writeln!(output, "  {}", terminal.badge(status, tone));
            } else {
                let _ = writeln!(
                    output,
                    "{:<22} {} {}  {}",
                    truncate(&display_model_name(&row.model), 22),
                    format_measure_human(units, terminal, false),
                    units.unit_name,
                    terminal.badge(status, tone)
                );
            }
        }
    }
    output.push('\n');
}

fn measure_badge(measure: &CostMeasure) -> (&'static str, Tone) {
    match measure.status {
        MeasureStatus::Exact => ("EXACT", Tone::Success),
        MeasureStatus::Bounded => ("RANGE", Tone::Warning),
        MeasureStatus::Partial => ("AT LEAST", Tone::Warning),
        MeasureStatus::Unpriced => ("NOT PRICED", Tone::Error),
        MeasureStatus::Unavailable => ("UNAVAILABLE", Tone::Muted),
    }
}

fn format_measure_human(
    measure: &CostMeasure,
    terminal: &TerminalOptions,
    currency: bool,
) -> String {
    let amount = |value| format_decimal(value, currency);
    match measure.status {
        MeasureStatus::Exact => measure
            .lower_bound
            .map(amount)
            .unwrap_or_else(|| "Unavailable".to_string()),
        MeasureStatus::Bounded => match (measure.lower_bound, measure.upper_bound) {
            (Some(lower), Some(upper)) => format!(
                "{}{}{}",
                amount(lower),
                terminal.range_separator(),
                amount(upper)
            ),
            (Some(lower), None) => format!(
                "{}{}",
                if terminal.unicode { "≥" } else { ">=" },
                amount(lower)
            ),
            _ => "Estimated range".to_string(),
        },
        MeasureStatus::Partial => measure
            .lower_bound
            .map(|value| {
                format!(
                    "{}{}",
                    if terminal.unicode { "≥" } else { ">=" },
                    amount(value)
                )
            })
            .unwrap_or_else(|| "Partial".to_string()),
        MeasureStatus::Unpriced => "Not priced".to_string(),
        MeasureStatus::Unavailable => "Unavailable".to_string(),
    }
}

/// Render a self-contained share-safe cost report. The cost document contains
/// no prompt text, paths, or event/session/source identifiers, and this
/// renderer intentionally exposes no user billing evidence IDs or notes.
pub fn render_cost_html(document: &CostDocument) -> String {
    let start = document
        .query
        .period
        .requested_start_date
        .as_deref()
        .unwrap_or("first matching local event");
    let end = document
        .query
        .period
        .requested_end_date_inclusive
        .as_deref()
        .unwrap_or("last matching local event");
    let api_value = escape_html(&format_measure(&document.combined.api_equivalent_usd, "$"));
    let recorded_cash = document
        .billing
        .recorded_cash_usd
        .map(decimal_display)
        .map(|value| format!("${value}"))
        .unwrap_or_else(|| "Unavailable".to_string());
    let actual_cash = document
        .billing
        .actual_billed_usd
        .map(decimal_display)
        .map(|value| format!("${value}"))
        .unwrap_or_else(|| "Unknown".to_string());
    let mut html = String::with_capacity(32_000);
    let _ = write!(
        html,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Token Ledger cost — {} through {}</title><style>{}</style></head><body><main><header><p class=\"eyebrow\">LOCAL INFERENCE ACCOUNTING / COST V1</p><h1>What did this usage cost?</h1><p class=\"lede\">A catalog-backed estimate of locally persisted Claude Code and OpenAI Codex activity. API-equivalent value, provider units, and cash billing evidence remain separate.</p><div class=\"window\"><b>{}</b><span>through</span><b>{}</b><small>{}</small></div></header>",
        escape_html(start),
        escape_html(end),
        COST_HTML_STYLE,
        escape_html(start),
        escape_html(end),
        escape_html(&document.query.timezone)
    );
    if document.coverage.provisional {
        let _ = write!(
            html,
            "<aside class=\"alert\"><b>PROVISIONAL SNAPSHOT</b> {} active or volatile source(s); totals may move after another scan.</aside>",
            document.coverage.active_or_volatile_source_count
        );
    }
    let _ = write!(
        html,
        "<section class=\"metrics\"><article><span>REQUESTS</span><strong>{}</strong><small>{} sessions</small></article><article><span>INPUT TOKENS</span><strong>{}</strong><small>{} cached · {} uncached</small></article><article><span>OUTPUT TOKENS</span><strong>{}</strong><small>{} cache-write tokens</small></article><article class=\"accent\"><span>API EQUIVALENT</span><strong>{}</strong><small>{:?} · not money paid</small></article></section>",
        format_count(document.combined.requests),
        format_count(document.combined.sessions),
        format_count(document.combined.usage.input_tokens_total),
        format_count(document.combined.usage.input_tokens_cached),
        format_count(document.combined.usage.input_tokens_uncached),
        format_count(document.combined.usage.output_tokens_total),
        format_count(document.combined.usage.cache_write_tokens()),
        api_value,
        document.combined.api_equivalent_usd.status,
    );
    html.push_str("<section><div class=\"section-head\"><div><p class=\"eyebrow\">01 / MODELS</p><h2>Cost register</h2></div><p>USD is an API list-price equivalent. Provider units remain in their native unit and are never merged across unlike units.</p></div>");
    if document.models.is_empty() {
        html.push_str("<div class=\"empty\"><b>NO MATCHING LOCAL EVENTS</b><p>This is not automatically a verified zero.</p></div>");
    } else {
        html.push_str("<div class=\"table-wrap\"><table><thead><tr><th>Client / model</th><th class=\"num\">Requests</th><th class=\"num\">Sessions</th><th class=\"num\">Input</th><th class=\"num\">Output</th><th class=\"num\">API equivalent</th><th>Provider units</th></tr></thead><tbody>");
        for row in &document.models {
            let units = if row.provider_units.is_empty() {
                "N/A".to_string()
            } else {
                row.provider_units
                    .iter()
                    .map(|value| format!("{} {}", format_measure(value, ""), value.unit_name))
                    .collect::<Vec<_>>()
                    .join(" · ")
            };
            let _ = write!(
                html,
                "<tr><td><b>{}</b><small>{}</small></td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num cost\">{}</td><td class=\"provider-units\">{}</td></tr>",
                escape_html(&row.model),
                escape_html(&row.client),
                format_count(row.requests),
                format_count(row.sessions),
                format_count(row.usage.input_tokens_total),
                format_count(row.usage.output_tokens_total),
                escape_html(&format_measure(&row.api_equivalent_usd, "$")),
                escape_html(&units),
            );
        }
        html.push_str("</tbody></table></div>");
    }
    html.push_str("</section>");
    let billing_status = document
        .billing
        .actual_billing_status
        .map(|value| format!("{value:?}"))
        .unwrap_or_else(|| "Unavailable".to_string());
    let _ = write!(
        html,
        "<section><div class=\"section-head\"><div><p class=\"eyebrow\">02 / BILLING</p><h2>Cash evidence</h2></div><p>These values come only from user-attested cash records, never from token-price arithmetic.</p></div><div class=\"split\"><article><span>RECORDED CASH USD</span><strong>{}</strong><small>Recorded evidence in the selected provider/time scope</small></article><article><span>ACTUAL BILLED USD</span><strong>{}</strong><small>{} · shown numerically only when attested complete</small></article></div><p class=\"note\">{}</p></section>",
        escape_html(&recorded_cash),
        escape_html(&actual_cash),
        escape_html(&billing_status),
        escape_html(&document.billing.note),
    );
    let recon = &document.reconciliation.summary;
    let _ = write!(
        html,
        "<section><div class=\"section-head\"><div><p class=\"eyebrow\">03 / RECONCILIATION</p><h2>Provider comparison</h2></div><p>Imported evidence is a comparison layer and never replaces local totals.</p></div><div class=\"recon\"><b>{} provider bucket(s)</b><span>{} matched</span><span>{} local only</span><span>{} provider only</span><span>{} counter mismatch</span><span>{} route unknown</span></div><p class=\"note\">{}</p>",
        document.reconciliation.selected_provider_bucket_count,
        recon.matched,
        recon.local_only,
        recon.provider_only,
        recon.counter_mismatch,
        recon.route_unknown,
        escape_html(&document.reconciliation.note),
    );
    if !document.reconciliation.limitations.is_empty() {
        html.push_str("<details><summary>Reconciliation limitations</summary><ul>");
        for limitation in &document.reconciliation.limitations {
            let _ = write!(html, "<li>{}</li>", escape_html(limitation));
        }
        html.push_str("</ul></details>");
    }
    html.push_str("</section>");
    html.push_str("<section><div class=\"section-head\"><div><p class=\"eyebrow\">04 / COVERAGE</p><h2>Local evidence boundary</h2></div><p>Blank intervals are unknown, not verified zeroes.</p></div><div class=\"coverage\">");
    for client in &document.coverage.clients {
        let _ = write!(
            html,
            "<article><b>{}</b><strong>{:?}</strong><span>{} matching event(s)</span><small>{}</small></article>",
            escape_html(display_client(client.client)),
            client.status,
            format_count(client.matching_event_count),
            escape_html(&client.note),
        );
    }
    let _ = write!(
        html,
        "</div></section><section><div class=\"section-head\"><div><p class=\"eyebrow\">05 / CATALOG</p><h2>Pricing evidence</h2></div><p>Immutable revision <b>{}</b> · SHA-256 <code>{}</code> · {:?}</p></div><ul class=\"sources\">",
        escape_html(&document.catalog.status.revision),
        escape_html(&document.catalog.status.sha256),
        document.catalog.status.freshness,
    );
    for source in &document.catalog.sources {
        let _ = write!(
            html,
            "<li><a href=\"{}\">{}</a><small>retrieved {}</small></li>",
            escape_html(&source.url),
            escape_html(&source.title),
            escape_html(&source.retrieved_at.to_rfc3339()),
        );
    }
    let _ = write!(
        html,
        "</ul></section><footer><div><b>INTERPRETATION</b><br>{}</div><div class=\"privacy\">SHARE-SAFE DEFAULT<br><small>No prompts, paths, event IDs, session IDs, source IDs, or billing evidence IDs.</small></div><div>Generated {}<br>As of {}</div></footer></main></body></html>",
        escape_html(&document.interpretation.join(" ")),
        escape_html(&document.generated_at_utc.to_rfc3339()),
        escape_html(
            &document
                .coverage
                .as_of
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "unavailable".to_string())
        ),
    );
    html
}

pub fn write_cost_html(document: &CostDocument, path: Option<&Path>) -> Result<String> {
    let html = render_cost_html(document);
    if let Some(path) = path {
        std::fs::write(path, &html)
            .with_context(|| format!("failed to write cost HTML {}", path.display()))?;
    }
    Ok(html)
}

const COST_HTML_STYLE: &str = r#"
:root{--ink:#161b18;--paper:#f4f0e6;--panel:#fffdf6;--line:#c8c0ad;--acid:#d7ff3f;--orange:#ff6b35;--muted:#5f665f}*{box-sizing:border-box}body{margin:0;background:#222820;color:var(--ink);font-family:ui-monospace,SFMono-Regular,Consolas,monospace}main{max-width:1240px;margin:24px auto;background:var(--paper);box-shadow:0 20px 70px #0008;border-top:10px solid var(--acid)}header{position:relative;padding:52px 56px 40px;border-bottom:2px solid var(--ink)}h1{max-width:780px;margin:.15em 0;font-family:Arial Black,Impact,sans-serif;font-size:clamp(42px,7vw,92px);line-height:.9;text-transform:uppercase;letter-spacing:-.06em}.lede{max-width:720px;font:18px/1.5 Arial,sans-serif;color:var(--muted)}.eyebrow{margin:0;font-weight:900;letter-spacing:.12em;color:#3a4d14}.window{position:absolute;right:56px;top:52px;display:grid;text-align:right;gap:4px;padding:16px;border:2px solid var(--ink);background:var(--panel);transform:rotate(1deg)}.window span,.window small{color:var(--muted)}section{padding:38px 56px;border-bottom:2px solid var(--ink)}.alert{margin:24px 56px 0;padding:14px 18px;background:#fff0d2;border:2px solid var(--orange)}.metrics{display:grid;grid-template-columns:repeat(4,1fr);padding:0}.metrics article,.split article{padding:28px;border-right:1px solid var(--line);background:var(--panel)}.metrics article:last-child{border-right:0}.metrics .accent{background:var(--acid)}article span,article small{display:block}.metrics strong,.split strong{display:block;margin:.2em 0;font:clamp(25px,3vw,42px)/1 Arial Black,Impact,sans-serif}.section-head{display:flex;justify-content:space-between;align-items:end;gap:30px;margin-bottom:24px}.section-head h2{margin:3px 0;font:34px/1 Arial Black,Impact,sans-serif;text-transform:uppercase}.section-head>p{max-width:520px;color:var(--muted);font:15px/1.5 Arial,sans-serif}.table-wrap{overflow:auto;border:2px solid var(--ink)}table{width:100%;border-collapse:collapse;background:var(--panel)}th,td{padding:13px 14px;border-bottom:1px solid var(--line);text-align:left;white-space:nowrap}th{background:var(--ink);color:white;font-size:12px;letter-spacing:.05em}td small{display:block;color:var(--muted);margin-top:4px}.num{text-align:right}.cost{font-weight:900}.provider-units{min-width:180px;max-width:220px;white-space:normal;overflow-wrap:anywhere;line-height:1.45}.split{display:grid;grid-template-columns:1fr 1fr;border:2px solid var(--ink)}.split article:last-child{border:0}.note{padding:14px 18px;border-left:6px solid var(--orange);background:var(--panel);font:15px/1.5 Arial,sans-serif}.recon{display:flex;flex-wrap:wrap;gap:10px}.recon>*{padding:10px 12px;border:1px solid var(--ink);background:var(--panel)}.coverage{display:grid;grid-template-columns:repeat(2,1fr);gap:16px}.coverage article{display:grid;gap:8px;padding:18px;border:2px solid var(--ink);background:var(--panel)}.coverage strong{text-transform:uppercase}.sources{display:grid;grid-template-columns:repeat(3,1fr);gap:12px;padding:0;list-style:none}.sources li{padding:14px;border:1px solid var(--line);background:var(--panel)}.sources a{color:var(--ink);font-weight:900}.sources small{display:block;margin-top:8px;color:var(--muted)}code{font-size:11px;word-break:break-all}.empty{padding:30px;border:2px dashed var(--ink);background:var(--panel)}footer{display:grid;grid-template-columns:2fr 1.3fr 1fr;gap:24px;padding:34px 56px;background:var(--ink);color:white;font:13px/1.5 Arial,sans-serif}.privacy{color:var(--acid);font-weight:900}@media(max-width:850px){main{margin:0}.window{position:static;text-align:left;transform:none;margin-top:24px}.metrics{grid-template-columns:1fr 1fr}.coverage,.sources,.split{grid-template-columns:1fr}.section-head{display:block}header,section{padding:32px 24px}.alert{margin:20px 24px 0}footer{grid-template-columns:1fr;padding:30px 24px}}
"#;

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::new();
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn format_measure(measure: &CostMeasure, prefix: &str) -> String {
    match measure.status {
        MeasureStatus::Exact => measure
            .lower_bound
            .map(decimal_display)
            .map(|value| format!("{prefix}{value} exact"))
            .unwrap_or_else(|| "unavailable".to_string()),
        MeasureStatus::Bounded => match (measure.lower_bound, measure.upper_bound) {
            (Some(lower), Some(upper)) => format!(
                "{prefix}{}–{prefix}{} bounded",
                decimal_display(lower),
                decimal_display(upper)
            ),
            (Some(lower), None) => format!(">={prefix}{}", decimal_display(lower)),
            _ => "bounded".to_string(),
        },
        MeasureStatus::Partial => measure
            .lower_bound
            .map(decimal_display)
            .map(|value| format!(">={prefix}{value} partial"))
            .unwrap_or_else(|| "partial".to_string()),
        MeasureStatus::Unpriced => "unpriced".to_string(),
        MeasureStatus::Unavailable => "n/a".to_string(),
    }
}

fn decimal_display(value: Decimal) -> String {
    value.round_dp(6).normalize().to_string()
}

fn display_client(client: Client) -> &'static str {
    match client {
        Client::ClaudeCode => "claude",
        Client::OpenaiCodex => "codex",
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let mut result: String = value.chars().take(width.saturating_sub(1)).collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClientCoverageSnapshot, CoverageStatus, PricingDimensions, UsageQuality};
    use chrono::TimeZone;

    fn event(
        client: Client,
        session: &str,
        model: &str,
        occurred_at: DateTime<Utc>,
    ) -> CanonicalEvent {
        CanonicalEvent {
            event_id: format!("event-{session}"),
            event_key: format!("key-{session}"),
            client,
            session_id: session.to_string(),
            provider_message_id: None,
            occurred_at,
            raw_model: model.to_string(),
            provider: match client {
                Client::ClaudeCode => "anthropic",
                Client::OpenaiCodex => "openai",
            }
            .to_string(),
            usage: UsageVector::default(),
            dimensions: PricingDimensions {
                auth_mode: (client == Client::OpenaiCodex).then(|| "chatgpt".to_string()),
                service_tier: (client == Client::OpenaiCodex).then(|| "standard".to_string()),
                speed: Some("standard".to_string()),
                cache_write_data_complete: Some(true),
                input_subset_accounting_consistent: Some(true),
                ..Default::default()
            },
            quality: UsageQuality::Exact,
            coverage: CoverageStatus::CompleteKnown,
            source_count: 1,
            warnings: Vec::new(),
        }
    }

    fn coverage() -> LedgerCoverageSnapshot {
        LedgerCoverageSnapshot {
            generated_at: Utc::now(),
            as_of: Some(Utc.with_ymd_and_hms(2026, 7, 11, 0, 0, 0).unwrap()),
            active_or_volatile_source_count: 0,
            provisional: false,
            last_scan: None,
            clients: Client::ALL
                .into_iter()
                .map(|client| ClientCoverageSnapshot {
                    client,
                    window_status: CoverageWindowStatus::ObservedWindow,
                    source_count: 1,
                    observation_count: 1,
                    canonical_event_count: 1,
                    warning_count: 0,
                    last_successful_source_scan: None,
                    earliest_canonical_event: None,
                    latest_canonical_event: None,
                })
                .collect(),
            warning_counts: Vec::new(),
        }
    }

    fn empty_extras() -> (CostBilling, CostReconciliation) {
        (
            CostBilling::unavailable("test"),
            CostReconciliation::unavailable("test"),
        )
    }

    #[test]
    fn all_history_empty_has_no_invented_bounds() -> Result<()> {
        let period = resolve_cost_period(
            CostPeriodSelection::AllLocalHistory,
            chrono_tz::UTC,
            Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap(),
            &[],
        )?;
        assert_eq!(period.kind, CostPeriodKind::AllLocalHistory);
        assert!(period.start_utc.is_none());
        assert!(period.end_utc_exclusive.is_none());
        assert!(period.requested_start_date.is_none());
        Ok(())
    }

    #[test]
    fn fable_geo_and_gpt_cache_write_bounds_remain_independent() -> Result<()> {
        let at = Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap();
        let mut fable = event(Client::ClaudeCode, "fable", "claude-fable-5", at);
        fable.usage.input_tokens_total = 1_000_000;
        fable.usage.input_tokens_uncached = 1_000_000;
        fable.usage.output_tokens_total = 1_000_000;
        fable.dimensions.inference_geo = Some("not_available".to_string());

        let mut gpt = event(Client::OpenaiCodex, "gpt", "gpt-5.6-sol", at);
        gpt.usage.input_tokens_total = 2_000_000;
        gpt.usage.input_tokens_uncached = 1_000_000;
        gpt.usage.input_tokens_cached = 1_000_000;
        gpt.dimensions.cache_write_data_complete = Some(false);

        let events = vec![fable, gpt];
        let period = resolve_cost_period(
            CostPeriodSelection::AllLocalHistory,
            chrono_tz::UTC,
            at,
            &events,
        )?;
        let query = CostQuery {
            period,
            timezone: "UTC".to_string(),
            client_filters: Vec::new(),
            model_filters: Vec::new(),
            scope_note: "test".to_string(),
        };
        let pricing = PricingEngine::bundled()?;
        let (billing, reconciliation) = empty_extras();
        let document = build_cost_document(
            &events,
            query,
            &pricing,
            coverage(),
            billing,
            reconciliation,
        )?;
        assert_eq!(document.models.len(), 2);
        let fable = document
            .models
            .iter()
            .find(|row| row.model == "claude-fable-5")
            .unwrap();
        assert_eq!(fable.api_equivalent_usd.status, MeasureStatus::Bounded);
        assert_eq!(
            fable.api_equivalent_usd.lower_bound,
            Some(Decimal::from(60))
        );
        assert_eq!(
            fable.api_equivalent_usd.upper_bound,
            Some(Decimal::from(66))
        );
        let gpt = document
            .models
            .iter()
            .find(|row| row.model == "gpt-5.6-sol")
            .unwrap();
        assert_eq!(gpt.api_equivalent_usd.status, MeasureStatus::Bounded);
        assert_eq!(
            gpt.api_equivalent_usd.lower_bound,
            Some(Decimal::new(55, 1))
        );
        assert_eq!(
            gpt.api_equivalent_usd.upper_bound,
            Some(Decimal::new(675, 2))
        );
        assert_eq!(gpt.provider_units.len(), 1);
        assert_eq!(gpt.provider_units[0].status, MeasureStatus::Exact);
        assert_eq!(document.combined.requests, 2);
        assert_eq!(document.combined.sessions, 2);
        assert_eq!(
            document.combined.api_equivalent_usd.lower_bound,
            Some(Decimal::new(655, 1))
        );
        assert_eq!(
            document.combined.api_equivalent_usd.upper_bound,
            Some(Decimal::new(7275, 2))
        );
        Ok(())
    }

    #[test]
    fn aggregate_evidence_compacts_event_level_arithmetic() -> Result<()> {
        let mut aggregate = MeasureAccumulator::default();
        for quantity in 1..=5_000_u64 {
            aggregate.add(&PricingMeasureEstimate {
                rate_kind: crate::pricing::RateKind::UsdApiEquivalent,
                unit_name: "USD".to_string(),
                status: MeasureStatus::Exact,
                lower_bound: Some(Decimal::ONE),
                upper_bound: Some(Decimal::ONE),
                dimension_evidence: Vec::new(),
                missing_components: Vec::new(),
                explanation: vec![
                    format!(
                        "output: {quantity} × 10 USD / 1000000 = 0.00001 USD."
                    ),
                    "API-equivalent USD is a public list-price estimate, not an invoice or subscription charge."
                        .to_string(),
                ],
            })?;
        }

        let measure = aggregate.finish("USD");
        assert_eq!(measure.evidence.explanation_count, 10_000);
        assert_eq!(measure.evidence.explanations.len(), 2);
        assert_eq!(measure.evidence.omitted_explanation_count, 9_998);
        assert!(measure.evidence.explanations.iter().any(|value| {
            value == "Per-event output arithmetic was aggregated into this measure's bounds."
        }));
        assert!(serde_json::to_vec(&measure)?.len() < 2_000);
        Ok(())
    }
}
