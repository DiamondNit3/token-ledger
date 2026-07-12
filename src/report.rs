use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::model::{
    CanonicalEvent, Client, CoverageWindowStatus, LedgerCoverageSnapshot, UsageQuality, UsageVector,
};
use crate::pricing::{
    CatalogFreshness, CatalogStatus, EstimateStatus, MeasureStatus, PricingEngine,
    PricingMeasureEstimate,
};
use crate::terminal::{
    Layout, TerminalOptions, Tone, display_client_name, display_model_name, format_count,
    format_count_compact, format_decimal, format_percent,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct GroupBy {
    pub day: bool,
    pub client: bool,
    pub model: bool,
}

impl GroupBy {
    pub fn day_model_client() -> Self {
        Self {
            day: true,
            client: true,
            model: true,
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        let mut result = Self::default();
        for item in value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            match item {
                "day" | "date" => result.day = true,
                "client" => result.client = true,
                "model" => result.model = true,
                other => anyhow::bail!("unsupported group '{other}'; use day,client,model"),
            }
        }
        if !result.day && !result.client && !result.model {
            anyhow::bail!("at least one grouping dimension is required");
        }
        Ok(result)
    }

    pub fn labels(self) -> Vec<String> {
        let mut labels = Vec::new();
        if self.day {
            labels.push("day".to_string());
        }
        if self.client {
            labels.push("client".to_string());
        }
        if self.model {
            labels.push("model".to_string());
        }
        labels
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey {
    day: Option<NaiveDate>,
    client: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Aggregate {
    event_ids: Vec<String>,
    sessions: HashSet<(Client, String)>,
    usage: UsageVector,
    api_usd_measure: MeasureAggregate,
    provider_unit_measure: MeasureAggregate,
    provider_unit_name: Option<String>,
    provider_units: BTreeMap<String, Decimal>,
    known_provider_units: BTreeMap<String, Decimal>,
    provider_expected_events: BTreeMap<String, u64>,
    provider_exact_events: BTreeMap<String, u64>,
    priced_events: u64,
    partial_events: u64,
    unpriced_events: u64,
    exact_events: u64,
    derived_events: u64,
    heuristic_events: u64,
    unresolved_events: u64,
    warnings: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
struct MeasureAggregate {
    exact_events: u64,
    bounded_events: u64,
    partial_events: u64,
    unpriced_events: u64,
    unavailable_events: u64,
    lower_bound: Decimal,
    upper_bound: Decimal,
    events_with_lower_bound: u64,
    events_with_upper_bound: u64,
}

impl MeasureAggregate {
    fn add(&mut self, measure: &PricingMeasureEstimate) {
        self.add_values(measure.status, measure.lower_bound, measure.upper_bound);
    }

    fn add_values(
        &mut self,
        status: MeasureStatus,
        lower_bound: Option<Decimal>,
        upper_bound: Option<Decimal>,
    ) {
        match status {
            MeasureStatus::Exact => self.exact_events += 1,
            MeasureStatus::Bounded => self.bounded_events += 1,
            MeasureStatus::Partial => self.partial_events += 1,
            MeasureStatus::Unpriced => self.unpriced_events += 1,
            MeasureStatus::Unavailable => self.unavailable_events += 1,
        }
        if let Some(lower) = lower_bound {
            self.lower_bound += lower;
            self.events_with_lower_bound += 1;
        }
        if let Some(upper) = upper_bound {
            self.upper_bound += upper;
            self.events_with_upper_bound += 1;
        }
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

    fn lower(&self) -> Option<String> {
        (self.events_with_lower_bound > 0).then(|| decimal_display(self.lower_bound))
    }

    fn upper(&self) -> Option<String> {
        let applicable = self.applicable_events();
        (applicable > 0 && self.events_with_upper_bound == applicable)
            .then(|| decimal_display(self.upper_bound))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportRow {
    pub day: Option<String>,
    pub client: Option<String>,
    pub model: Option<String>,
    pub requests: u64,
    /// Canonical event IDs for `token-ledger explain --event <ID>`.
    pub event_ids: Vec<String>,
    pub sessions: u64,
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
    pub api_equivalent_usd: Option<String>,
    pub known_api_equivalent_usd: Option<String>,
    pub api_equivalent_usd_status: MeasureStatus,
    pub api_equivalent_usd_lower_bound: Option<String>,
    pub api_equivalent_usd_upper_bound: Option<String>,
    pub provider_units: BTreeMap<String, String>,
    pub known_provider_units: BTreeMap<String, String>,
    pub provider_unit_name: Option<String>,
    pub provider_unit_status: MeasureStatus,
    pub provider_unit_lower_bound: Option<String>,
    pub provider_unit_upper_bound: Option<String>,
    pub priced_events: u64,
    pub partial_events: u64,
    pub unpriced_events: u64,
    pub quality: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryCoverageStatus {
    NoSources,
    NoObservations,
    MatchingEvents,
    NoEventsWithinObservedWindow,
    OutsideObservedWindow,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryCoverageAssessment {
    pub client: Client,
    pub status: QueryCoverageStatus,
    pub matching_event_count: u64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportQuery {
    pub requested_start_date: String,
    pub requested_end_date_inclusive: String,
    pub timezone: String,
    pub start_utc: DateTime<Utc>,
    pub end_utc_exclusive: DateTime<Utc>,
    pub group_by: Vec<String>,
    /// Optional client filters applied before aggregation.
    pub client_filters: Vec<String>,
    /// Optional raw or catalog-canonical model filters applied before aggregation.
    pub model_filters: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportDocument {
    pub schema_version: String,
    pub generated_at_utc: DateTime<Utc>,
    pub query: ReportQuery,
    pub catalog: CatalogStatus,
    pub coverage: LedgerCoverageSnapshot,
    pub query_coverage: Vec<QueryCoverageAssessment>,
    pub coverage_note: String,
    pub rows: Vec<ReportRow>,
}

#[allow(clippy::too_many_arguments)]
pub fn build_report_document(
    events: &[CanonicalEvent],
    rows: Vec<ReportRow>,
    requested_start_date: NaiveDate,
    requested_end_date_inclusive: NaiveDate,
    timezone: Tz,
    start_utc: DateTime<Utc>,
    end_utc_exclusive: DateTime<Utc>,
    group_by: GroupBy,
    catalog: CatalogStatus,
    coverage: LedgerCoverageSnapshot,
) -> ReportDocument {
    let query_coverage = coverage
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
                    "Persisted local events matched the requested interval.".to_string(),
                ),
                CoverageWindowStatus::ObservedWindow => {
                    let outside = snapshot
                        .earliest_canonical_event
                        .as_ref()
                        .zip(snapshot.latest_canonical_event.as_ref())
                        .is_some_and(|(earliest, latest)| {
                            end_utc_exclusive <= earliest.occurred_at
                                || start_utc > latest.occurred_at
                        });
                    if outside {
                        (
                            QueryCoverageStatus::OutsideObservedWindow,
                            "The requested interval is outside this ledger's observed local event window."
                                .to_string(),
                        )
                    } else {
                        (
                            QueryCoverageStatus::NoEventsWithinObservedWindow,
                            "No persisted events matched inside the broader observed window; this is not proof of zero provider usage."
                                .to_string(),
                        )
                    }
                }
            };
            QueryCoverageAssessment {
                client: snapshot.client,
                status,
                matching_event_count,
                note,
            }
        })
        .collect();

    ReportDocument {
        schema_version: "token-ledger.report.v2".to_string(),
        generated_at_utc: Utc::now(),
        query: ReportQuery {
            requested_start_date: requested_start_date.to_string(),
            requested_end_date_inclusive: requested_end_date_inclusive.to_string(),
            timezone: timezone.to_string(),
            start_utc,
            end_utc_exclusive,
            group_by: group_by.labels(),
            client_filters: Vec::new(),
            model_filters: Vec::new(),
        },
        catalog,
        coverage,
        query_coverage,
        coverage_note: "Coverage describes readable local persistence, not a provider invoice or proof that no other usage occurred. Deleted, expired, ephemeral, remote, or other-machine sessions can remain missing."
            .to_string(),
        rows,
    }
}

pub fn aggregate(
    events: &[CanonicalEvent],
    timezone: Tz,
    group_by: GroupBy,
    pricing: &PricingEngine,
) -> Vec<ReportRow> {
    let mut groups: BTreeMap<GroupKey, Aggregate> = BTreeMap::new();
    for event in events {
        let key = GroupKey {
            day: group_by
                .day
                .then(|| event.occurred_at.with_timezone(&timezone).date_naive()),
            client: group_by
                .client
                .then(|| display_client(event.client).to_string()),
            model: group_by.model.then(|| canonical_model_name(pricing, event)),
        };
        let aggregate = groups.entry(key).or_default();
        aggregate.event_ids.push(event.event_id.clone());
        aggregate
            .sessions
            .insert((event.client, event.session_id.clone()));
        add_usage(&mut aggregate.usage, &event.usage);
        match event.quality {
            UsageQuality::Exact => aggregate.exact_events += 1,
            UsageQuality::Derived => aggregate.derived_events += 1,
            UsageQuality::Heuristic => aggregate.heuristic_events += 1,
            UsageQuality::Unresolved => aggregate.unresolved_events += 1,
        }
        aggregate.warnings.extend(event.warnings.iter().cloned());

        let estimate = pricing.estimate_event(event);
        aggregate
            .api_usd_measure
            .add(&estimate.api_equivalent_usd_measure);
        aggregate
            .provider_unit_measure
            .add(&estimate.provider_unit_measure);
        if event.client == Client::OpenaiCodex {
            aggregate.provider_unit_name = Some(estimate.provider_unit_measure.unit_name.clone());
        }
        if let Some(unit) = estimate.provider_unit_name.clone() {
            *aggregate
                .provider_expected_events
                .entry(unit.clone())
                .or_default() += 1;
            if let Some(amount) = estimate.provider_units {
                *aggregate.provider_units.entry(unit.clone()).or_default() += amount;
                *aggregate
                    .known_provider_units
                    .entry(unit.clone())
                    .or_default() += amount;
                *aggregate.provider_exact_events.entry(unit).or_default() += 1;
            } else if let Some(amount) = estimate.known_provider_units {
                *aggregate.known_provider_units.entry(unit).or_default() += amount;
            }
        }
        match estimate.status {
            EstimateStatus::Priced => aggregate.priced_events += 1,
            EstimateStatus::Partial => {
                aggregate.partial_events += 1;
                aggregate.warnings.insert(
                    "one or more events have partial price coverage; use `token-ledger explain` for event-level missing components"
                        .to_string(),
                );
            }
            EstimateStatus::Unpriced => {
                aggregate.unpriced_events += 1;
                aggregate.warnings.insert(
                    "one or more events are unpriced; use `token-ledger explain` for event-level rate matching details"
                        .to_string(),
                );
            }
        }
        match estimate.api_equivalent_usd_measure.status {
            MeasureStatus::Bounded => {
                aggregate.warnings.insert(
                    "API-equivalent USD is a finite scenario range, not a single exact amount"
                        .to_string(),
                );
            }
            MeasureStatus::Partial => {
                aggregate.warnings.insert(
                    "API-equivalent USD has a lower bound but no verified finite upper bound"
                        .to_string(),
                );
            }
            _ => {}
        }
        if estimate.provider_unit_measure.status == MeasureStatus::Bounded {
            aggregate.warnings.insert(
                "provider units are bounded because a pricing dimension was not source-observed"
                    .to_string(),
            );
        }
    }

    groups
        .into_iter()
        .map(|(key, aggregate)| {
            let request_count = aggregate.event_ids.len() as u64;
            let api_usd_status = aggregate.api_usd_measure.status();
            let api_usd_lower = aggregate.api_usd_measure.lower();
            let api_usd_upper = aggregate.api_usd_measure.upper();
            let provider_unit_status = aggregate.provider_unit_measure.status();
            let provider_unit_lower = aggregate.provider_unit_measure.lower();
            let provider_unit_upper = aggregate.provider_unit_measure.upper();
            let exact_provider_units: BTreeSet<_> = aggregate
                .provider_expected_events
                .iter()
                .filter(|(unit, expected)| {
                    aggregate.provider_exact_events.get(*unit) == Some(*expected)
                })
                .map(|(unit, _)| unit.clone())
                .collect();
            let quality = if aggregate.unresolved_events > 0 {
                "unresolved"
            } else if aggregate.heuristic_events > 0 {
                "heuristic"
            } else if aggregate.derived_events > 0 {
                "derived"
            } else {
                "exact"
            };
            let mut event_ids = aggregate.event_ids;
            event_ids.sort();
            event_ids.dedup();
            ReportRow {
                day: key.day.map(|value| value.to_string()),
                client: key.client,
                model: key.model,
                requests: request_count,
                event_ids,
                sessions: aggregate.sessions.len() as u64,
                input_tokens_total: aggregate.usage.input_tokens_total,
                input_tokens_uncached: aggregate.usage.input_tokens_uncached,
                input_tokens_cached: aggregate.usage.input_tokens_cached,
                cache_write_5m_tokens: aggregate.usage.cache_write_5m_tokens,
                cache_write_1h_tokens: aggregate.usage.cache_write_1h_tokens,
                cache_write_unknown_tokens: aggregate.usage.cache_write_unknown_tokens,
                output_tokens_total: aggregate.usage.output_tokens_total,
                reasoning_output_tokens: aggregate.usage.reasoning_output_tokens,
                web_search_requests: aggregate.usage.web_search_requests,
                web_fetch_requests: aggregate.usage.web_fetch_requests,
                api_equivalent_usd: (api_usd_status == MeasureStatus::Exact)
                    .then(|| api_usd_lower.clone())
                    .flatten(),
                known_api_equivalent_usd: (api_usd_status != MeasureStatus::Exact)
                    .then(|| api_usd_lower.clone())
                    .flatten(),
                api_equivalent_usd_status: api_usd_status,
                api_equivalent_usd_lower_bound: api_usd_lower,
                api_equivalent_usd_upper_bound: api_usd_upper,
                provider_units: aggregate
                    .provider_units
                    .into_iter()
                    .filter(|(unit, _)| exact_provider_units.contains(unit))
                    .map(|(unit, amount)| (unit, decimal_display(amount)))
                    .collect(),
                provider_unit_name: aggregate.provider_unit_name,
                provider_unit_status,
                provider_unit_lower_bound: provider_unit_lower,
                provider_unit_upper_bound: provider_unit_upper,
                known_provider_units: aggregate
                    .known_provider_units
                    .into_iter()
                    .filter(|(unit, amount)| {
                        !exact_provider_units.contains(unit) && !amount.is_zero()
                    })
                    .map(|(unit, amount)| (unit, decimal_display(amount)))
                    .collect(),
                priced_events: aggregate.priced_events,
                partial_events: aggregate.partial_events,
                unpriced_events: aggregate.unpriced_events,
                quality: quality.to_string(),
                warnings: aggregate.warnings.into_iter().collect(),
            }
        })
        .collect()
}

/// Compact totals across a report's non-overlapping aggregate rows.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReportSummary {
    pub requests: u64,
    pub input_tokens_total: u64,
    pub input_tokens_uncached: u64,
    pub input_tokens_cached: u64,
    pub cache_write_tokens: u64,
    pub output_tokens_total: u64,
    pub priced_events: u64,
    pub partial_events: u64,
    pub unpriced_events: u64,
    pub api_equivalent_usd: Option<String>,
    pub known_api_equivalent_usd: Option<String>,
    pub api_equivalent_usd_status: MeasureStatus,
    pub api_equivalent_usd_lower_bound: Option<String>,
    pub api_equivalent_usd_upper_bound: Option<String>,
    pub provider_unit_name: Option<String>,
    pub provider_unit_status: MeasureStatus,
    pub provider_unit_lower_bound: Option<String>,
    pub provider_unit_upper_bound: Option<String>,
}

pub fn summarize_rows(rows: &[ReportRow]) -> ReportSummary {
    let mut summary = ReportSummary::default();
    let mut api_measure = MeasureAggregate::default();
    let mut provider_measure = MeasureAggregate::default();

    for row in rows {
        summary.requests = summary.requests.saturating_add(row.requests);
        summary.input_tokens_total = summary
            .input_tokens_total
            .saturating_add(row.input_tokens_total);
        summary.input_tokens_uncached = summary
            .input_tokens_uncached
            .saturating_add(row.input_tokens_uncached);
        summary.input_tokens_cached = summary
            .input_tokens_cached
            .saturating_add(row.input_tokens_cached);
        summary.cache_write_tokens = summary.cache_write_tokens.saturating_add(
            row.cache_write_5m_tokens
                .saturating_add(row.cache_write_1h_tokens)
                .saturating_add(row.cache_write_unknown_tokens),
        );
        summary.output_tokens_total = summary
            .output_tokens_total
            .saturating_add(row.output_tokens_total);
        summary.priced_events = summary.priced_events.saturating_add(row.priced_events);
        summary.partial_events = summary.partial_events.saturating_add(row.partial_events);
        summary.unpriced_events = summary.unpriced_events.saturating_add(row.unpriced_events);

        api_measure.add_values(
            row.api_equivalent_usd_status,
            parse_decimal(row.api_equivalent_usd_lower_bound.as_deref()),
            parse_decimal(row.api_equivalent_usd_upper_bound.as_deref()),
        );
        provider_measure.add_values(
            row.provider_unit_status,
            parse_decimal(row.provider_unit_lower_bound.as_deref()),
            parse_decimal(row.provider_unit_upper_bound.as_deref()),
        );
        if summary.provider_unit_name.is_none() {
            summary.provider_unit_name = row.provider_unit_name.clone();
        }
    }

    summary.api_equivalent_usd_status = api_measure.status();
    summary.api_equivalent_usd_lower_bound = api_measure.lower();
    summary.api_equivalent_usd_upper_bound = api_measure.upper();
    summary.api_equivalent_usd = (summary.api_equivalent_usd_status == MeasureStatus::Exact)
        .then(|| summary.api_equivalent_usd_lower_bound.clone())
        .flatten();
    summary.known_api_equivalent_usd = (summary.api_equivalent_usd_status != MeasureStatus::Exact)
        .then(|| summary.api_equivalent_usd_lower_bound.clone())
        .flatten();
    summary.provider_unit_status = provider_measure.status();
    summary.provider_unit_lower_bound = provider_measure.lower();
    summary.provider_unit_upper_bound = provider_measure.upper();
    summary
}

pub fn render_table(rows: &[ReportRow], timezone: Tz, catalog_version: &str) -> String {
    render_table_with_options(rows, timezone, catalog_version, &TerminalOptions::default())
}

pub fn render_table_with_options(
    rows: &[ReportRow],
    timezone: Tz,
    catalog_version: &str,
    terminal: &TerminalOptions,
) -> String {
    let mut output = String::new();
    let summary = summarize_rows(rows);
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(Tone::Accent, "TOKEN LEDGER / USAGE")
    );
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(
            Tone::Muted,
            format!(
                "Timezone {timezone}{}catalog {catalog_version}",
                terminal.separator()
            )
        )
    );
    render_report_summary(&summary, terminal, &mut output);
    render_report_rows(rows, terminal, &mut output);
    output
}

pub fn render_report(document: &ReportDocument) -> String {
    render_report_with_options(document, &TerminalOptions::default())
}

pub fn render_report_with_options(document: &ReportDocument, terminal: &TerminalOptions) -> String {
    let timezone: Tz = document.query.timezone.parse().unwrap_or(chrono_tz::UTC);
    let mut output = String::new();
    let summary = summarize_rows(&document.rows);
    let sep = terminal.separator();
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
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(Tone::Accent, "TOKEN LEDGER / USAGE")
    );
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(
            Tone::Muted,
            format!(
                "{} through {}{sep}{timezone}",
                document.query.requested_start_date, document.query.requested_end_date_inclusive
            )
        )
    );
    let catalog_tone = if document.catalog.verification.error_count() == 0 {
        Tone::Success
    } else {
        Tone::Error
    };
    if terminal.layout() == Layout::Narrow {
        let _ = writeln!(
            output,
            "Catalog {} {}",
            document.catalog.revision,
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
            document.catalog.revision,
            terminal.badge("VERIFIED", catalog_tone),
            terminal.badge(snapshot_label, snapshot_tone)
        );
    }
    render_report_summary(&summary, terminal, &mut output);
    render_report_rows(&document.rows, terminal, &mut output);

    if !document.query.client_filters.is_empty() || !document.query.model_filters.is_empty() {
        let clients = if document.query.client_filters.is_empty() {
            "all".to_string()
        } else {
            document.query.client_filters.join(", ")
        };
        let models = if document.query.model_filters.is_empty() {
            "all".to_string()
        } else {
            document.query.model_filters.join(", ")
        };
        let _ = writeln!(
            output,
            "Filters: clients {clients}{}models {models}",
            terminal.separator()
        );
    }
    if document.rows.is_empty() {
        let _ = writeln!(
            output,
            "No persisted usage events matched this interval. This is not automatically a verified zero."
        );
    }
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
            format_count(document.coverage.active_or_volatile_source_count)
        );
    }
    for assessment in &document.query_coverage {
        if terminal.details || assessment.status != QueryCoverageStatus::MatchingEvents {
            let snapshot = document
                .coverage
                .clients
                .iter()
                .find(|value| value.client == assessment.client);
            let window = snapshot
                .and_then(|value| {
                    value
                        .earliest_canonical_event
                        .as_ref()
                        .zip(value.latest_canonical_event.as_ref())
                })
                .map(|(earliest, latest)| {
                    format!("{} .. {}", earliest.occurred_at, latest.occurred_at)
                })
                .unwrap_or_else(|| "unavailable".to_string());
            let tone = if assessment.status == QueryCoverageStatus::MatchingEvents {
                Tone::Success
            } else {
                Tone::Warning
            };
            if terminal.layout() == Layout::Narrow {
                let _ = writeln!(
                    output,
                    "{} {}",
                    terminal.paint(tone, terminal.status_symbol(tone)),
                    display_client_name(assessment.client.as_str())
                );
                let _ = writeln!(output, "  {}", query_coverage_label(assessment.status));
                let _ = writeln!(
                    output,
                    "  Matching events: {}",
                    format_count(assessment.matching_event_count)
                );
                if terminal.details {
                    let _ = writeln!(output, "  Observed window: {window}");
                }
            } else {
                let _ = writeln!(
                    output,
                    "{} {:<12} {:<30}{}{} event(s){}{}",
                    terminal.paint(tone, terminal.status_symbol(tone)),
                    display_client_name(assessment.client.as_str()),
                    query_coverage_label(assessment.status),
                    sep,
                    format_count(assessment.matching_event_count),
                    if terminal.details { sep } else { "" },
                    if terminal.details { &window } else { "" }
                );
            }
        }
    }
    if document.catalog.freshness != CatalogFreshness::Fresh {
        let _ = writeln!(
            output,
            "{} Price catalog is {}; inspect `token-ledger prices status` before relying on estimates.",
            terminal.paint(Tone::Warning, terminal.status_symbol(Tone::Warning)),
            catalog_freshness_label(document.catalog.freshness)
        );
    }
    if terminal.details {
        let _ = writeln!(output, "Catalog SHA-256: {}", document.catalog.sha256);
        let _ = writeln!(output, "Note: {}", document.coverage_note);
        if !document.rows.is_empty() {
            let _ = writeln!(
                output,
                "Each row has a privacy-safe event reference for `token-ledger explain --event <ID>`."
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

fn render_report_summary(summary: &ReportSummary, terminal: &TerminalOptions, output: &mut String) {
    output.push('\n');
    let price = report_measure_human(
        summary.api_equivalent_usd_status,
        summary.api_equivalent_usd_lower_bound.as_deref(),
        summary.api_equivalent_usd_upper_bound.as_deref(),
        terminal,
        true,
    );
    let (label, tone) = report_measure_badge(summary.api_equivalent_usd_status);
    let _ = writeln!(
        output,
        "{}  {}",
        terminal.paint(Tone::Strong, price),
        terminal.badge(label, tone)
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
    let tokens = summary
        .input_tokens_total
        .saturating_add(summary.output_tokens_total);
    match terminal.layout() {
        Layout::Wide => {
            let _ = writeln!(
                output,
                "{} tokens{}{} requests{}{} cached{}{} output",
                terminal.paint(Tone::Strong, format_count_compact(tokens)),
                terminal.separator(),
                terminal.paint(Tone::Strong, format_count(summary.requests)),
                terminal.separator(),
                terminal.paint(
                    Tone::Strong,
                    format_percent(summary.input_tokens_cached, summary.input_tokens_total)
                ),
                terminal.separator(),
                terminal.paint(
                    Tone::Strong,
                    format_count_compact(summary.output_tokens_total)
                )
            );
        }
        Layout::Compact | Layout::Narrow => {
            let _ = writeln!(output, "Tokens:   {}", format_count(tokens));
            let _ = writeln!(output, "Requests: {}", format_count(summary.requests));
            let _ = writeln!(
                output,
                "Cached:   {}",
                format_percent(summary.input_tokens_cached, summary.input_tokens_total)
            );
        }
    }
    output.push('\n');
}

fn render_report_rows(rows: &[ReportRow], terminal: &TerminalOptions, output: &mut String) {
    let _ = writeln!(output, "{}", terminal.paint(Tone::Accent, "BY MODEL"));
    if rows.is_empty() {
        let _ = writeln!(output, "No matching persisted usage events.");
        output.push('\n');
        return;
    }
    match terminal.layout() {
        Layout::Wide => {
            let _ = writeln!(
                output,
                "{:<10} {:<21} {:<12} {:>8} {:>11} {:>22}  STATUS",
                "DAY", "MODEL", "CLIENT", "REQUESTS", "TOKENS", "ESTIMATE"
            );
            let _ = writeln!(output, "{}", terminal.rule(104));
            for row in rows {
                let tokens = row
                    .input_tokens_total
                    .saturating_add(row.output_tokens_total);
                let price = report_measure_human(
                    row.api_equivalent_usd_status,
                    row.api_equivalent_usd_lower_bound.as_deref(),
                    row.api_equivalent_usd_upper_bound.as_deref(),
                    terminal,
                    true,
                );
                let (label, tone) = report_measure_badge(row.api_equivalent_usd_status);
                let _ = writeln!(
                    output,
                    "{:<10} {:<21} {:<12} {:>8} {:>11} {:>22}  {}",
                    truncate(row.day.as_deref().unwrap_or("All"), 10),
                    truncate(
                        &display_model_name(row.model.as_deref().unwrap_or("All models")),
                        21
                    ),
                    truncate(
                        &display_client_name(row.client.as_deref().unwrap_or("All clients")),
                        12
                    ),
                    format_count(row.requests),
                    format_count_compact(tokens),
                    price,
                    terminal.badge(label, tone)
                );
                render_report_row_details(row, terminal, output);
            }
        }
        Layout::Compact => {
            for row in rows {
                let tokens = row
                    .input_tokens_total
                    .saturating_add(row.output_tokens_total);
                let (label, tone) = report_measure_badge(row.api_equivalent_usd_status);
                let _ = writeln!(
                    output,
                    "{}{}{}  {}",
                    terminal.paint(
                        Tone::Strong,
                        display_model_name(row.model.as_deref().unwrap_or("All models"))
                    ),
                    terminal.separator(),
                    display_client_name(row.client.as_deref().unwrap_or("All clients")),
                    terminal.badge(label, tone)
                );
                let _ = writeln!(
                    output,
                    "  {} requests{}{} tokens{}{}",
                    format_count(row.requests),
                    terminal.separator(),
                    format_count_compact(tokens),
                    terminal.separator(),
                    report_measure_human(
                        row.api_equivalent_usd_status,
                        row.api_equivalent_usd_lower_bound.as_deref(),
                        row.api_equivalent_usd_upper_bound.as_deref(),
                        terminal,
                        true,
                    )
                );
                render_report_row_details(row, terminal, output);
            }
        }
        Layout::Narrow => {
            for row in rows {
                let tokens = row
                    .input_tokens_total
                    .saturating_add(row.output_tokens_total);
                let (label, tone) = report_measure_badge(row.api_equivalent_usd_status);
                let _ = writeln!(
                    output,
                    "{}",
                    display_model_name(row.model.as_deref().unwrap_or("All models"))
                );
                let _ = writeln!(
                    output,
                    "  Client:   {}",
                    display_client_name(row.client.as_deref().unwrap_or("All clients"))
                );
                if let Some(day) = row.day.as_deref() {
                    let _ = writeln!(output, "  Day:      {day}");
                }
                let _ = writeln!(output, "  Requests: {}", format_count(row.requests));
                let _ = writeln!(output, "  Tokens:   {}", format_count(tokens));
                let _ = writeln!(
                    output,
                    "  Estimate: {} {}",
                    report_measure_human(
                        row.api_equivalent_usd_status,
                        row.api_equivalent_usd_lower_bound.as_deref(),
                        row.api_equivalent_usd_upper_bound.as_deref(),
                        terminal,
                        true,
                    ),
                    terminal.badge(label, tone)
                );
                render_report_row_details(row, terminal, output);
            }
        }
    }
    if rows.iter().any(|row| row.unpriced_events > 0) {
        let _ = writeln!(
            output,
            "{} Unpriced events are excluded from USD totals; they are never treated as $0.",
            terminal.paint(Tone::Warning, terminal.status_symbol(Tone::Warning))
        );
    }
    output.push('\n');
}

fn render_report_row_details(row: &ReportRow, terminal: &TerminalOptions, output: &mut String) {
    if !terminal.details {
        return;
    }
    let writes = row
        .cache_write_5m_tokens
        .saturating_add(row.cache_write_1h_tokens)
        .saturating_add(row.cache_write_unknown_tokens);
    let _ = writeln!(
        output,
        "  Input {} uncached{}{} cached{}{} writes{}{} output{}quality {}",
        format_count(row.input_tokens_uncached),
        terminal.separator(),
        format_count(row.input_tokens_cached),
        terminal.separator(),
        format_count(writes),
        terminal.separator(),
        format_count(row.output_tokens_total),
        terminal.separator(),
        row.quality
    );
    if let Some(event_id) = row.event_ids.first() {
        let _ = writeln!(output, "  Inspect: token-ledger explain --event {event_id}");
    }
}

fn report_measure_badge(status: MeasureStatus) -> (&'static str, Tone) {
    match status {
        MeasureStatus::Exact => ("EXACT", Tone::Success),
        MeasureStatus::Bounded => ("RANGE", Tone::Warning),
        MeasureStatus::Partial => ("AT LEAST", Tone::Warning),
        MeasureStatus::Unpriced => ("NOT PRICED", Tone::Error),
        MeasureStatus::Unavailable => ("UNAVAILABLE", Tone::Muted),
    }
}

fn report_measure_human(
    status: MeasureStatus,
    lower: Option<&str>,
    upper: Option<&str>,
    terminal: &TerminalOptions,
    currency: bool,
) -> String {
    let lower = parse_decimal(lower);
    let upper = parse_decimal(upper);
    let amount = |value| format_decimal(value, currency);
    match status {
        MeasureStatus::Exact => lower
            .map(amount)
            .unwrap_or_else(|| "Unavailable".to_string()),
        MeasureStatus::Bounded => match (lower, upper) {
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
        MeasureStatus::Partial => lower
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

fn query_coverage_label(status: QueryCoverageStatus) -> &'static str {
    match status {
        QueryCoverageStatus::NoSources => "no sources",
        QueryCoverageStatus::NoObservations => "no observations",
        QueryCoverageStatus::MatchingEvents => "matching persisted events",
        QueryCoverageStatus::NoEventsWithinObservedWindow => "no events in observed window",
        QueryCoverageStatus::OutsideObservedWindow => "outside observed window",
    }
}

fn catalog_freshness_label(freshness: CatalogFreshness) -> &'static str {
    match freshness {
        CatalogFreshness::Fresh => "fresh",
        CatalogFreshness::Stale => "stale",
        CatalogFreshness::FutureDated => "future-dated",
    }
}

pub fn write_json(rows: &[ReportRow], path: Option<&Path>) -> Result<String> {
    let text = serde_json::to_string_pretty(rows)?;
    if let Some(path) = path {
        std::fs::write(path, &text)
            .with_context(|| format!("failed to write JSON export {}", path.display()))?;
    }
    Ok(text)
}

pub fn write_report_json(document: &ReportDocument, path: Option<&Path>) -> Result<String> {
    let text = serde_json::to_string_pretty(document)?;
    if let Some(path) = path {
        std::fs::write(path, &text)
            .with_context(|| format!("failed to write JSON report {}", path.display()))?;
    }
    Ok(text)
}

const ROW_CSV_HEADERS: [&str; 32] = [
    "day",
    "client",
    "model",
    "requests",
    "event_ids_json",
    "sessions",
    "input_tokens_total",
    "input_tokens_uncached",
    "input_tokens_cached",
    "cache_write_5m_tokens",
    "cache_write_1h_tokens",
    "cache_write_unknown_tokens",
    "output_tokens_total",
    "reasoning_output_tokens",
    "web_search_requests",
    "web_fetch_requests",
    "api_equivalent_usd",
    "known_api_equivalent_usd",
    "api_equivalent_usd_status",
    "api_equivalent_usd_lower_bound",
    "api_equivalent_usd_upper_bound",
    "provider_units_json",
    "known_provider_units_json",
    "provider_unit_name",
    "provider_unit_status",
    "provider_unit_lower_bound",
    "provider_unit_upper_bound",
    "priced_events",
    "partial_events",
    "unpriced_events",
    "quality",
    "warnings_json",
];

fn row_csv_record(row: &ReportRow) -> Result<Vec<String>> {
    Ok(vec![
        row.day.as_deref().unwrap_or_default().to_string(),
        row.client.as_deref().unwrap_or_default().to_string(),
        row.model.as_deref().unwrap_or_default().to_string(),
        row.requests.to_string(),
        serde_json::to_string(&row.event_ids)?,
        row.sessions.to_string(),
        row.input_tokens_total.to_string(),
        row.input_tokens_uncached.to_string(),
        row.input_tokens_cached.to_string(),
        row.cache_write_5m_tokens.to_string(),
        row.cache_write_1h_tokens.to_string(),
        row.cache_write_unknown_tokens.to_string(),
        row.output_tokens_total.to_string(),
        row.reasoning_output_tokens.to_string(),
        row.web_search_requests.to_string(),
        row.web_fetch_requests.to_string(),
        row.api_equivalent_usd.clone().unwrap_or_default(),
        row.known_api_equivalent_usd.clone().unwrap_or_default(),
        measure_status_label(row.api_equivalent_usd_status).to_string(),
        row.api_equivalent_usd_lower_bound
            .clone()
            .unwrap_or_default(),
        row.api_equivalent_usd_upper_bound
            .clone()
            .unwrap_or_default(),
        serde_json::to_string(&row.provider_units)?,
        serde_json::to_string(&row.known_provider_units)?,
        row.provider_unit_name.clone().unwrap_or_default(),
        measure_status_label(row.provider_unit_status).to_string(),
        row.provider_unit_lower_bound.clone().unwrap_or_default(),
        row.provider_unit_upper_bound.clone().unwrap_or_default(),
        row.priced_events.to_string(),
        row.partial_events.to_string(),
        row.unpriced_events.to_string(),
        row.quality.clone(),
        serde_json::to_string(&row.warnings)?,
    ])
}

fn finish_csv(writer: csv::Writer<Vec<u8>>, path: Option<&Path>) -> Result<String> {
    let bytes = writer.into_inner()?;
    let text = String::from_utf8(bytes).context("CSV writer produced invalid UTF-8")?;
    if let Some(path) = path {
        std::fs::write(path, &text)
            .with_context(|| format!("failed to write CSV export {}", path.display()))?;
    }
    Ok(text)
}

pub fn write_csv(rows: &[ReportRow], path: Option<&Path>) -> Result<String> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(ROW_CSV_HEADERS)?;
    for row in rows {
        writer.write_record(row_csv_record(row)?)?;
    }
    finish_csv(writer, path)
}

pub fn write_report_csv(document: &ReportDocument, path: Option<&Path>) -> Result<String> {
    const META_HEADERS: [&str; 14] = [
        "record_type",
        "report_schema_version",
        "generated_at_utc",
        "timezone",
        "requested_start_date",
        "requested_end_date_inclusive",
        "client_filters_json",
        "model_filters_json",
        "query_start_utc",
        "query_end_utc_exclusive",
        "catalog_revision",
        "catalog_sha256",
        "catalog_freshness",
        "coverage_json",
    ];
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(META_HEADERS.into_iter().chain(ROW_CSV_HEADERS))?;

    let coverage_json = serde_json::to_string(&serde_json::json!({
        "coverage": &document.coverage,
        "query_coverage": &document.query_coverage,
        "coverage_note": &document.coverage_note,
    }))?;
    let common = [
        document.schema_version.clone(),
        document.generated_at_utc.to_rfc3339(),
        document.query.timezone.clone(),
        document.query.requested_start_date.clone(),
        document.query.requested_end_date_inclusive.clone(),
        serde_json::to_string(&document.query.client_filters)?,
        serde_json::to_string(&document.query.model_filters)?,
        document.query.start_utc.to_rfc3339(),
        document.query.end_utc_exclusive.to_rfc3339(),
        document.catalog.revision.clone(),
        document.catalog.sha256.clone(),
        catalog_freshness_label(document.catalog.freshness).to_string(),
    ];
    let mut metadata_record = vec!["metadata".to_string()];
    metadata_record.extend(common.iter().cloned());
    metadata_record.push(coverage_json);
    metadata_record.extend((0..ROW_CSV_HEADERS.len()).map(|_| String::new()));
    writer.write_record(metadata_record)?;

    for row in &document.rows {
        let mut record = vec!["data".to_string()];
        record.extend(common.iter().cloned());
        record.push(String::new());
        record.extend(row_csv_record(row)?);
        writer.write_record(record)?;
    }
    finish_csv(writer, path)
}

pub fn local_day_bounds(date: NaiveDate, timezone: Tz) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let next = date.succ_opt().context("date is out of supported range")?;
    let start = resolve_local(timezone, date.and_hms_opt(0, 0, 0).unwrap())?;
    let end = resolve_local(timezone, next.and_hms_opt(0, 0, 0).unwrap())?;
    Ok((start.with_timezone(&Utc), end.with_timezone(&Utc)))
}

pub fn local_range_bounds(
    start: NaiveDate,
    end_inclusive: NaiveDate,
    timezone: Tz,
) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    if end_inclusive < start {
        anyhow::bail!("range end must be on or after start");
    }
    let start_at = resolve_local(timezone, start.and_hms_opt(0, 0, 0).unwrap())?;
    let after_end = end_inclusive
        .succ_opt()
        .context("range end is out of supported range")?;
    let end_at = resolve_local(timezone, after_end.and_hms_opt(0, 0, 0).unwrap())?;
    Ok((start_at.with_timezone(&Utc), end_at.with_timezone(&Utc)))
}

fn resolve_local(timezone: Tz, value: chrono::NaiveDateTime) -> Result<DateTime<Tz>> {
    match timezone.from_local_datetime(&value) {
        LocalResult::Single(value) => Ok(value),
        LocalResult::Ambiguous(earlier, _) => Ok(earlier),
        LocalResult::None => {
            for minutes in 1..=180 {
                let candidate = value + chrono::Duration::minutes(minutes);
                match timezone.from_local_datetime(&candidate) {
                    LocalResult::Single(value) | LocalResult::Ambiguous(value, _) => {
                        return Ok(value);
                    }
                    LocalResult::None => {}
                }
            }
            anyhow::bail!("could not resolve local midnight {value} in timezone {timezone}")
        }
    }
}

fn display_client(client: Client) -> &'static str {
    match client {
        Client::ClaudeCode => "claude",
        Client::OpenaiCodex => "codex",
    }
}

/// Resolve a raw model through the effective-dated catalog while preserving
/// the raw value when there is no single unambiguous alias.
pub fn canonical_model_name(pricing: &PricingEngine, event: &CanonicalEvent) -> String {
    let provider = event.provider.trim().to_ascii_lowercase();
    let matches = pricing
        .catalog()
        .aliases()
        .iter()
        .filter(|alias| {
            alias.provider == provider
                && alias.raw_model == event.raw_model
                && event.occurred_at >= alias.interval.effective_from
                && alias
                    .interval
                    .effective_to
                    .is_none_or(|end| event.occurred_at < end)
        })
        .map(|alias| alias.canonical_model.as_str())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [model] => (*model).to_string(),
        _ => event.raw_model.clone(),
    }
}

fn add_usage(target: &mut UsageVector, source: &UsageVector) {
    target.input_tokens_total = target
        .input_tokens_total
        .saturating_add(source.input_tokens_total);
    target.input_tokens_uncached = target
        .input_tokens_uncached
        .saturating_add(source.input_tokens_uncached);
    target.input_tokens_cached = target
        .input_tokens_cached
        .saturating_add(source.input_tokens_cached);
    target.cache_write_5m_tokens = target
        .cache_write_5m_tokens
        .saturating_add(source.cache_write_5m_tokens);
    target.cache_write_1h_tokens = target
        .cache_write_1h_tokens
        .saturating_add(source.cache_write_1h_tokens);
    target.cache_write_unknown_tokens = target
        .cache_write_unknown_tokens
        .saturating_add(source.cache_write_unknown_tokens);
    target.output_tokens_total = target
        .output_tokens_total
        .saturating_add(source.output_tokens_total);
    target.reasoning_output_tokens = target
        .reasoning_output_tokens
        .saturating_add(source.reasoning_output_tokens);
    target.web_search_requests = target
        .web_search_requests
        .saturating_add(source.web_search_requests);
    target.web_fetch_requests = target
        .web_fetch_requests
        .saturating_add(source.web_fetch_requests);
}

fn decimal_display(value: Decimal) -> String {
    value.round_dp(6).normalize().to_string()
}

fn parse_decimal(value: Option<&str>) -> Option<Decimal> {
    value.and_then(|value| value.parse::<Decimal>().ok())
}

fn measure_status_label(status: MeasureStatus) -> &'static str {
    match status {
        MeasureStatus::Exact => "exact",
        MeasureStatus::Bounded => "bounded",
        MeasureStatus::Partial => "partial",
        MeasureStatus::Unpriced => "unpriced",
        MeasureStatus::Unavailable => "unavailable",
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
    use chrono::TimeZone;

    fn event(client: Client, session_id: &str, model: &str) -> CanonicalEvent {
        CanonicalEvent {
            event_id: format!("{}-{session_id}", client.as_str()),
            event_key: format!("{}-{session_id}", client.as_str()),
            client,
            session_id: session_id.to_string(),
            provider_message_id: None,
            occurred_at: Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap(),
            raw_model: model.to_string(),
            provider: match client {
                Client::ClaudeCode => "anthropic",
                Client::OpenaiCodex => "openai",
            }
            .to_string(),
            usage: UsageVector::default(),
            dimensions: crate::model::PricingDimensions {
                auth_mode: (client == Client::OpenaiCodex).then(|| "chatgpt".to_string()),
                cache_write_data_complete: Some(true),
                ..Default::default()
            },
            quality: UsageQuality::Exact,
            coverage: crate::model::CoverageStatus::CompleteKnown,
            source_count: 1,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn dst_days_have_correct_utc_span() -> Result<()> {
        let tz: Tz = "America/New_York".parse()?;
        let spring = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
        let fall = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let (spring_start, spring_end) = local_day_bounds(spring, tz)?;
        let (fall_start, fall_end) = local_day_bounds(fall, tz)?;
        assert_eq!((spring_end - spring_start).num_hours(), 23);
        assert_eq!((fall_end - fall_start).num_hours(), 25);
        Ok(())
    }

    #[test]
    fn sessions_are_distinct_across_clients_and_exports_include_fetches() -> Result<()> {
        let timezone: Tz = "UTC".parse()?;
        let mut claude = event(Client::ClaudeCode, "shared", "unknown");
        claude.usage.web_fetch_requests = 2;
        let mut codex = event(Client::OpenaiCodex, "shared", "unknown");
        codex.usage.web_fetch_requests = 3;
        let pricing = PricingEngine::bundled()?;

        let rows = aggregate(
            &[claude, codex],
            timezone,
            GroupBy {
                day: true,
                client: false,
                model: false,
            },
            &pricing,
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sessions, 2);
        assert_eq!(rows[0].web_fetch_requests, 5);
        Ok(())
    }

    #[test]
    fn exact_usd_is_not_downgraded_by_missing_provider_units() -> Result<()> {
        let timezone: Tz = "UTC".parse()?;
        let mut codex = event(Client::OpenaiCodex, "session", "gpt-5-codex");
        codex.usage.input_tokens_total = 1_000_000;
        codex.usage.input_tokens_uncached = 1_000_000;
        codex.dimensions.service_tier = Some("standard".to_string());
        let pricing = PricingEngine::bundled()?;

        let rows = aggregate(&[codex], timezone, GroupBy::day_model_client(), &pricing);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].partial_events, 1);
        assert_eq!(rows[0].api_equivalent_usd.as_deref(), Some("1.25"));
        assert_eq!(rows[0].known_api_equivalent_usd, None);
        assert_eq!(rows[0].api_equivalent_usd_status, MeasureStatus::Exact);
        assert_eq!(rows[0].provider_unit_status, MeasureStatus::Unpriced);
        assert!(rows[0].provider_units.is_empty());
        assert!(rows[0].known_provider_units.is_empty());
        Ok(())
    }

    #[test]
    fn fable_geo_scenarios_are_aggregated_as_a_finite_usd_range() -> Result<()> {
        let timezone: Tz = "UTC".parse()?;
        let mut fable = event(Client::ClaudeCode, "session", "claude-fable-5");
        fable.usage.input_tokens_total = 1_000_000;
        fable.usage.input_tokens_uncached = 1_000_000;
        fable.usage.output_tokens_total = 1_000_000;
        fable.dimensions.inference_geo = Some("not_available".to_string());
        let pricing = PricingEngine::bundled()?;

        let rows = aggregate(&[fable], timezone, GroupBy::day_model_client(), &pricing);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].api_equivalent_usd_status, MeasureStatus::Bounded);
        assert_eq!(
            rows[0].api_equivalent_usd_lower_bound.as_deref(),
            Some("60")
        );
        assert_eq!(
            rows[0].api_equivalent_usd_upper_bound.as_deref(),
            Some("66")
        );
        assert_eq!(rows[0].api_equivalent_usd, None);
        assert_eq!(rows[0].known_api_equivalent_usd.as_deref(), Some("60"));

        let summary = summarize_rows(&rows);
        assert_eq!(summary.api_equivalent_usd_status, MeasureStatus::Bounded);
        assert_eq!(
            summary.api_equivalent_usd_lower_bound.as_deref(),
            Some("60")
        );
        assert_eq!(
            summary.api_equivalent_usd_upper_bound.as_deref(),
            Some("66")
        );
        let human = render_table(&rows, timezone, pricing.catalog().revision());
        assert!(human.contains("$60.00–$66.00"));
        let csv = write_csv(&rows, None)?;
        assert!(csv.lines().next().is_some_and(|header| {
            header.contains("api_equivalent_usd_status") && header.contains("provider_unit_status")
        }));
        assert!(csv.contains(",bounded,60,66,"));
        Ok(())
    }

    #[test]
    fn populated_csv_flattens_maps_and_warnings_as_json() -> Result<()> {
        let timezone: Tz = "UTC".parse()?;
        let mut codex = event(Client::OpenaiCodex, "session", "gpt-5.4");
        codex.usage.input_tokens_total = 1_000;
        codex.usage.input_tokens_uncached = 1_000;
        codex.usage.output_tokens_total = 100;
        codex.warnings.push("bounded warning".into());
        let pricing = PricingEngine::bundled()?;
        let mut rows = aggregate(&[codex], timezone, GroupBy::day_model_client(), &pricing);
        rows[0]
            .provider_units
            .insert("Codex credits".into(), "1.25".into());
        assert!(!rows[0].provider_units.is_empty());

        let text = write_csv(&rows, None)?;
        let mut reader = csv::Reader::from_reader(text.as_bytes());
        let headers = reader.headers()?.clone();
        let record = reader
            .records()
            .next()
            .context("CSV did not contain its populated data row")??;
        let units_index = headers
            .iter()
            .position(|value| value == "provider_units_json")
            .context("provider_units_json header is missing")?;
        let warnings_index = headers
            .iter()
            .position(|value| value == "warnings_json")
            .context("warnings_json header is missing")?;
        let units: BTreeMap<String, String> = serde_json::from_str(&record[units_index])?;
        let warnings: Vec<String> = serde_json::from_str(&record[warnings_index])?;

        assert!(!units.is_empty());
        assert!(warnings.iter().any(|value| value == "bounded warning"));
        Ok(())
    }

    #[test]
    fn model_grouping_uses_effective_catalog_aliases() -> Result<()> {
        let timezone: Tz = "UTC".parse()?;
        let raw_alias = event(Client::OpenaiCodex, "one", "gpt-5.6");
        let canonical = event(Client::OpenaiCodex, "two", "gpt-5.6-sol");
        let pricing = PricingEngine::bundled()?;

        let rows = aggregate(
            &[raw_alias, canonical],
            timezone,
            GroupBy::day_model_client(),
            &pricing,
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(rows[0].requests, 2);
        Ok(())
    }
}
