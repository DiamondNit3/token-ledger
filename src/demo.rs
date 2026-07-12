//! Deterministic, isolated data for the public CLI walkthrough.
//!
//! The demo deliberately constructs accounting envelopes in memory. It does
//! not load configuration, open a ledger, discover source roots, or invoke an
//! adapter. This keeps `token-ledger demo` safe to run before initialization.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;

use crate::billing::{BillingCategory, BillingEvidence, BillingWindow, OneTimeBillingRecord};
use crate::cost::{
    CostBilling, CostDocument, CostPeriodSelection, CostQuery, CostReconciliation,
    build_cost_document, render_cost_with_options, resolve_cost_period,
};
use crate::model::{
    CanonicalEvent, Client, ClientCoverageSnapshot, CoverageEventBoundary, CoverageStatus,
    CoverageWindowStatus, LedgerCoverageSnapshot, PricingDimensions, UsageQuality, UsageVector,
};
use crate::pricing::PricingEngine;
use crate::terminal::{TerminalOptions, Tone};

const DEMO_SCOPE_NOTE: &str = "Deterministic synthetic accounting envelopes used only for the Token Ledger walkthrough. No local configuration, database, session root, transcript, or provider export was read.";
const DEMO_BILLING_NOTE: &str = "Synthetic demo record; not a real charge or invoice.";

/// Render the deterministic walkthrough through the production cost renderer.
pub fn render_demo(terminal: &TerminalOptions) -> Result<String> {
    let document = synthetic_cost_document()?;
    let report = render_cost_with_options(&document, terminal);
    let mut lines = report.lines();
    // Replace the production COST heading while preserving every subsequent
    // line from the real renderer.
    let _ = lines.next();

    let mut output = String::new();
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(Tone::Accent, "TOKEN LEDGER / DEMO")
    );
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(
            Tone::Muted,
            "Synthetic data only; no config, database, or session roots were read."
        )
    );
    for line in lines {
        let _ = writeln!(output, "{line}");
    }
    output.push('\n');
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(Tone::Accent, "TRY IT WITH YOUR DATA")
    );
    let _ = writeln!(output, "  token-ledger today");
    let _ = writeln!(output, "  token-ledger cost --month");
    Ok(output)
}

/// Build a fixed cost document without consulting any process or filesystem
/// state. This is public so integration and downstream presentation tests can
/// assert the demo's accounting shape directly.
pub fn synthetic_cost_document() -> Result<CostDocument> {
    let generated_at = utc(2026, 7, 11, 16, 0, 0);
    let start_date = NaiveDate::from_ymd_opt(2026, 7, 1).expect("valid demo start date");
    let end_date = NaiveDate::from_ymd_opt(2026, 7, 11).expect("valid demo end date");
    let mut events = Vec::new();

    for index in 0..12_u32 {
        let mut event = synthetic_event(
            Client::ClaudeCode,
            &format!("claude-{}", index % 3 + 1),
            "claude-fable-5",
            utc(2026, 7, 2, 13, 0, 0) + Duration::hours(i64::from(index) * 11),
            index,
        );
        event.usage = UsageVector {
            input_tokens_total: 2_400_000,
            input_tokens_uncached: 600_000,
            input_tokens_cached: 1_800_000,
            output_tokens_total: 120_000,
            ..Default::default()
        };
        // The official catalog exposes global and US-only scenarios. Leaving
        // geography unavailable intentionally demonstrates a bounded estimate.
        event.dimensions.inference_geo = Some("not_available".to_string());
        events.push(event);
    }

    for index in 0..10_u32 {
        let mut event = synthetic_event(
            Client::OpenaiCodex,
            &format!("codex-{}", index % 3 + 1),
            "gpt-5.6-sol",
            utc(2026, 7, 10, 13, 30, 0) + Duration::minutes(i64::from(index) * 45),
            index,
        );
        event.usage = UsageVector {
            input_tokens_total: 1_800_000,
            input_tokens_uncached: 500_000,
            input_tokens_cached: 1_300_000,
            output_tokens_total: 80_000,
            reasoning_output_tokens: 20_000,
            ..Default::default()
        };
        event.dimensions.auth_mode = Some("chatgpt".to_string());
        // An intentionally incomplete cache-write dimension exercises the
        // range treatment without inventing usage.
        event.dimensions.cache_write_data_complete = Some(false);
        events.push(event);
    }

    let pricing = PricingEngine::bundled()?;
    let period = resolve_cost_period(
        CostPeriodSelection::ExplicitRange {
            start: start_date,
            end: end_date,
        },
        chrono_tz::UTC,
        generated_at,
        &events,
    )?;
    let query = CostQuery {
        period,
        timezone: "UTC".to_string(),
        client_filters: Vec::new(),
        model_filters: Vec::new(),
        scope_note: DEMO_SCOPE_NOTE.to_string(),
    };

    let providers = BTreeSet::from(["anthropic".to_string(), "openai".to_string()]);
    let billing_window = BillingWindow::new(utc(2026, 7, 1, 0, 0, 0), utc(2026, 7, 12, 0, 0, 0))?;
    let billing_evidence = BillingEvidence {
        one_time_charges: vec![
            synthetic_charge("demo-anthropic", "anthropic", 20, utc(2026, 7, 2, 9, 0, 0)),
            synthetic_charge("demo-openai", "openai", 20, utc(2026, 7, 2, 9, 5, 0)),
        ],
        ..Default::default()
    };
    let billing = CostBilling::from_evidence(&billing_evidence, billing_window, &providers)?;
    let reconciliation = CostReconciliation::unavailable(
        "Synthetic demo does not load or imply provider reconciliation evidence.",
    );

    let mut document = build_cost_document(
        &events,
        query,
        &pricing,
        synthetic_coverage(&events, generated_at),
        billing,
        reconciliation,
    )?;
    // `build_cost_document` timestamps normal reports at execution time. The
    // walkthrough fixes that envelope so every invocation is byte-stable.
    document.generated_at_utc = generated_at;
    Ok(document)
}

fn synthetic_event(
    client: Client,
    session: &str,
    model: &str,
    occurred_at: DateTime<Utc>,
    index: u32,
) -> CanonicalEvent {
    let client_slug = match client {
        Client::ClaudeCode => "claude",
        Client::OpenaiCodex => "codex",
    };
    CanonicalEvent {
        event_id: format!("demo-{client_slug}-event-{index:02}"),
        event_key: format!("demo-{client_slug}-key-{index:02}"),
        client,
        session_id: format!("demo-{session}"),
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
            service_tier: Some("standard".to_string()),
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

fn synthetic_charge(
    id: &str,
    provider: &str,
    amount_usd: i64,
    charged_at: DateTime<Utc>,
) -> OneTimeBillingRecord {
    OneTimeBillingRecord {
        id: id.to_string(),
        provider: provider.to_string(),
        category: BillingCategory::SubscriptionPlan,
        amount_usd: Decimal::from(amount_usd),
        charged_at,
        attested_at: charged_at + Duration::hours(1),
        source_note: DEMO_BILLING_NOTE.to_string(),
    }
}

fn synthetic_coverage(
    events: &[CanonicalEvent],
    generated_at: DateTime<Utc>,
) -> LedgerCoverageSnapshot {
    let clients = Client::ALL
        .into_iter()
        .map(|client| {
            let matching = events
                .iter()
                .filter(|event| event.client == client)
                .collect::<Vec<_>>();
            let earliest = matching
                .iter()
                .min_by_key(|event| event.occurred_at)
                .expect("demo has events for every client");
            let latest = matching
                .iter()
                .max_by_key(|event| event.occurred_at)
                .expect("demo has events for every client");
            ClientCoverageSnapshot {
                client,
                window_status: CoverageWindowStatus::ObservedWindow,
                source_count: match client {
                    Client::ClaudeCode => 4,
                    Client::OpenaiCodex => 5,
                },
                observation_count: matching.len() as u64,
                canonical_event_count: matching.len() as u64,
                warning_count: 0,
                last_successful_source_scan: None,
                earliest_canonical_event: Some(CoverageEventBoundary {
                    event_id: earliest.event_id.clone(),
                    occurred_at: earliest.occurred_at,
                }),
                latest_canonical_event: Some(CoverageEventBoundary {
                    event_id: latest.event_id.clone(),
                    occurred_at: latest.occurred_at,
                }),
            }
        })
        .collect();
    LedgerCoverageSnapshot {
        generated_at,
        as_of: Some(generated_at),
        active_or_volatile_source_count: 0,
        provisional: false,
        last_scan: None,
        clients,
        warning_counts: Vec::new(),
    }
}

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
        .single()
        .expect("valid deterministic demo timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::MeasureStatus;

    #[test]
    fn synthetic_document_is_fixed_and_exercises_both_clients() -> Result<()> {
        let document = synthetic_cost_document()?;
        assert_eq!(document.generated_at_utc, utc(2026, 7, 11, 16, 0, 0));
        assert_eq!(document.combined.requests, 22);
        assert_eq!(document.combined.sessions, 6);
        assert_eq!(document.models.len(), 2);
        assert_eq!(document.billing.recorded_cash_usd, Some(Decimal::from(40)));
        assert_eq!(document.billing.actual_billed_usd, None);
        assert_eq!(
            document.combined.api_equivalent_usd.status,
            MeasureStatus::Bounded
        );
        assert!(
            document
                .models
                .iter()
                .any(|row| row.model == "claude-fable-5")
        );
        assert!(document.models.iter().any(|row| row.model == "gpt-5.6-sol"));
        Ok(())
    }

    #[test]
    fn plain_render_is_deterministic_and_self_identifying() -> Result<()> {
        let terminal = TerminalOptions::plain(120);
        let first = render_demo(&terminal)?;
        let second = render_demo(&terminal)?;
        assert_eq!(first, second);
        assert!(first.contains("TOKEN LEDGER / DEMO"));
        assert!(first.contains("Synthetic data only"));
        assert!(first.contains("Claude Fable 5"));
        assert!(first.contains("GPT-5.6 Sol"));
        Ok(())
    }
}
