use std::fs;
use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use chrono::{TimeZone, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use tempfile::TempDir;
use token_ledger::billing::{
    BillingCategory, BillingCompletenessAttestation, BillingEvidence, OneTimeBillingRecord,
};
use token_ledger::config::{Config, PricingDimensionOverride};

const PRIVACY_CANARY: &str = "COST_PRIVATE_PROMPT_PATH_SESSION_72B91E";
const BILLING_CANARY: &str = "COST_PRIVATE_BILLING_NOTE_72B91E";

struct CostFixture {
    _temp: TempDir,
    config: PathBuf,
    database: PathBuf,
}

impl CostFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary cost fixture");
        let config = temp.path().join("config").join("ledger.toml");
        let database = temp.path().join("data").join("ledger.sqlite3");
        let claude_root = temp.path().join("claude-private-root");
        let codex_home = temp.path().join("codex-private-home");
        let claude_path = claude_root
            .join("projects")
            .join("private-project")
            .join("private-session.jsonl");
        let codex_path = codex_home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("10")
            .join("rollout-2026-07-10T12-00-00-22222222-2222-7222-8222-222222222222.jsonl");
        fs::create_dir_all(claude_path.parent().unwrap()).expect("Claude fixture parent");
        fs::create_dir_all(codex_path.parent().unwrap()).expect("Codex fixture parent");
        fs::write(
            &claude_path,
            format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-07-10T11:59:59Z\",\"sessionId\":\"private-claude-session\",\"message\":{{\"content\":\"{PRIVACY_CANARY}\"}}}}\n{{\"type\":\"assistant\",\"timestamp\":\"2026-07-10T12:00:00Z\",\"sessionId\":\"private-claude-session\",\"requestId\":\"private-request\",\"message\":{{\"id\":\"private-message\",\"model\":\"claude-fable-5\",\"content\":\"{PRIVACY_CANARY}\",\"usage\":{{\"input_tokens\":1000000,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":1000000,\"service_tier\":\"standard\",\"speed\":\"standard\",\"inference_geo\":\"not_available\"}}}}}}\n"
            ),
        )
        .expect("Claude fixture");
        fs::write(
            &codex_path,
            format!(
                "{{\"timestamp\":\"2026-07-10T12:30:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"22222222-2222-7222-8222-222222222222\",\"session_id\":\"private-codex-session\",\"model_provider\":\"openai\",\"auth_mode\":\"chatgpt\"}}}}\n{{\"timestamp\":\"2026-07-10T12:30:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"thread_settings_applied\",\"thread_settings\":{{\"model\":\"gpt-5.6-sol\",\"service_tier\":\"standard\"}}}}}}\n{{\"timestamp\":\"2026-07-10T12:30:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{PRIVACY_CANARY}\"}}]}}}}\n{{\"timestamp\":\"2026-07-10T12:30:03Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":2000000,\"cached_input_tokens\":1000000,\"output_tokens\":0,\"reasoning_output_tokens\":0,\"total_tokens\":2000000}},\"last_token_usage\":{{\"input_tokens\":2000000,\"cached_input_tokens\":1000000,\"output_tokens\":0,\"reasoning_output_tokens\":0,\"total_tokens\":2000000}},\"model_context_window\":200000}}}}}}\n"
            ),
        )
        .expect("Codex fixture");

        let fixture = Self {
            _temp: temp,
            config,
            database,
        };
        let output = fixture
            .command()
            .arg("--db")
            .arg(&fixture.database)
            .args(["init", "--tz", "UTC"])
            .output()
            .expect("initialize cost fixture");
        assert_success(&output, "init");
        let (mut config, _) = Config::load(Some(&fixture.config)).expect("load cost config");
        config.claude_root = Some(claude_root);
        config.codex_home = Some(codex_home);
        config.save(&fixture.config).expect("save source roots");
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("ledger").expect("compiled ledger");
        command.arg("--config").arg(&self.config);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command().args(args).output().expect("run ledger cost");
        assert_success(&output, &args.join(" "));
        output
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert_private(&output.stdout);
        serde_json::from_slice(&output.stdout).expect("cost JSON")
    }

    fn set_billing(&self, complete: bool) {
        let (mut config, _) = Config::load(Some(&self.config)).expect("load billing config");
        let charged_at = Utc.with_ymd_and_hms(2026, 7, 10, 18, 0, 0).unwrap();
        let attested_at = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        config.billing_evidence = BillingEvidence {
            one_time_charges: vec![
                OneTimeBillingRecord {
                    id: "openai-cash".to_string(),
                    provider: "openai".to_string(),
                    category: BillingCategory::SubscriptionPlan,
                    amount_usd: Decimal::from(25),
                    charged_at,
                    attested_at,
                    source_note: BILLING_CANARY.to_string(),
                },
                OneTimeBillingRecord {
                    id: "anthropic-cash".to_string(),
                    provider: "anthropic".to_string(),
                    category: BillingCategory::SubscriptionPlan,
                    amount_usd: Decimal::from(50),
                    charged_at,
                    attested_at,
                    source_note: BILLING_CANARY.to_string(),
                },
            ],
            recurring_plan_charges: Vec::new(),
            completeness_attestations: if complete {
                ["openai", "anthropic"]
                    .into_iter()
                    .map(|provider| BillingCompletenessAttestation {
                        id: format!("{provider}-complete"),
                        provider: provider.to_string(),
                        effective_from: Utc.with_ymd_and_hms(2026, 7, 10, 0, 0, 0).unwrap(),
                        effective_to: Utc.with_ymd_and_hms(2026, 7, 11, 0, 0, 0).unwrap(),
                        attested_at,
                        source_note: BILLING_CANARY.to_string(),
                    })
                    .collect()
            } else {
                Vec::new()
            },
        };
        config.save(&self.config).expect("save billing config");
    }
}

#[test]
fn cost_all_combines_models_with_independent_bounds_and_catalog_evidence() {
    let fixture = CostFixture::new();
    let cost = fixture.json(&[
        "cost",
        "--all",
        "--model",
        "gpt-5.6-sol",
        "--model",
        "claude-fable-5",
        "--json",
    ]);
    assert_eq!(cost["schema_version"], "token-ledger.cost.v1");
    assert_eq!(cost["query"]["period"]["kind"], "all_local_history");
    assert_eq!(
        cost["query"]["period"]["requested_start_date"],
        "2026-07-10"
    );
    assert_eq!(cost["catalog"]["status"]["revision"], "2026-07-10.3");
    assert!(cost["catalog"]["status"]["sha256"].as_str().is_some());
    assert!(!cost["catalog"]["sources"].as_array().unwrap().is_empty());
    assert_eq!(cost["combined"]["requests"], 2);
    assert_eq!(cost["combined"]["sessions"], 2);
    assert_eq!(cost["combined"]["usage"]["input_tokens_total"], 3_000_000);
    assert_eq!(cost["combined"]["usage"]["output_tokens_total"], 1_000_000);
    assert_eq!(cost["combined"]["api_equivalent_usd"]["status"], "bounded");
    assert_eq!(
        cost["combined"]["api_equivalent_usd"]["lower_bound"],
        "65.5"
    );
    assert_eq!(
        cost["combined"]["api_equivalent_usd"]["upper_bound"],
        "72.75"
    );
    let rows = cost["models"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let fable = rows
        .iter()
        .find(|row| row["model"] == "claude-fable-5")
        .unwrap();
    assert_eq!(fable["api_equivalent_usd"]["lower_bound"], "60");
    assert_eq!(fable["api_equivalent_usd"]["upper_bound"], "66.0");
    assert!(fable["provider_units"].as_array().unwrap().is_empty());
    let gpt = rows
        .iter()
        .find(|row| row["model"] == "gpt-5.6-sol")
        .unwrap();
    assert_eq!(gpt["api_equivalent_usd"]["lower_bound"], "5.5");
    assert_eq!(gpt["api_equivalent_usd"]["upper_bound"], "6.75");
    assert_eq!(gpt["provider_units"][0]["unit_name"], "Codex credits");
    assert_eq!(gpt["provider_units"][0]["status"], "exact");
    assert_eq!(gpt["provider_units"][0]["lower_bound"], "137.5");
    assert_eq!(cost["reconciliation"]["provider_evidence_present"], false);
    assert!(cost["coverage"]["as_of"].as_str().is_some());
    assert!(cost["coverage"]["provisional"].is_boolean());
}

#[test]
fn cost_filters_periods_html_and_human_output_are_private() {
    let fixture = CostFixture::new();
    fixture.run(&["scan"]);
    let claude = fixture.json(&[
        "cost",
        "--all",
        "--no-scan",
        "--client",
        "claude",
        "--model",
        "claude-fable-5",
        "--json",
    ]);
    assert_eq!(claude["models"].as_array().unwrap().len(), 1);
    assert_eq!(claude["models"][0]["client"], "claude");
    assert_eq!(claude["query"]["client_filters"][0], "claude_code");

    let explicit = fixture.json(&[
        "cost",
        "--start",
        "2026-07-10",
        "--end",
        "2026-07-10",
        "--no-scan",
        "--json",
    ]);
    assert_eq!(explicit["query"]["period"]["kind"], "explicit_range");
    assert_eq!(explicit["combined"]["requests"], 2);

    for selector in ["--today", "--yesterday", "--month"] {
        let selected = fixture.json(&["cost", selector, "--no-scan", "--json"]);
        assert_eq!(selected["schema_version"], "token-ledger.cost.v1");
    }

    fixture.set_billing(false);
    let html_path = fixture._temp.path().join("private-cost-report.html");
    let html_path_text = html_path.to_string_lossy().to_string();
    let output = fixture.run(&["cost", "--all", "--no-scan", "--html", &html_path_text]);
    assert_private(&output.stdout);
    let html = fs::read_to_string(&html_path).expect("cost HTML");
    assert!(html.contains("API EQUIVALENT"));
    assert!(html.contains("Cash evidence"));
    assert!(html.contains("Provider comparison"));
    assert!(html.contains("Pricing evidence"));
    assert!(!html.contains(PRIVACY_CANARY));
    assert!(!html.contains(BILLING_CANARY));
    assert!(!html.contains("openai-cash"));
    assert!(!html.contains("anthropic-cash"));
    assert!(!html.contains("private-claude-session"));
    assert!(!html.contains("private-codex-session"));
    assert!(!html.contains("claude-private-root"));
    assert!(!html.contains("codex-private-home"));
    assert!(!html.contains("evt_"));

    let human = fixture.run(&["cost", "--all", "--no-scan"]);
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains("Requests: 2") || human.contains("2 requests"));
    assert!(human.contains("Actual billed"));
    assert!(human.contains("not money paid"));
    assert!(!human.contains(PRIVACY_CANARY));
}

#[test]
fn cost_keeps_incomplete_and_attested_billing_separate_from_estimates() {
    let fixture = CostFixture::new();
    fixture.run(&["scan"]);
    fixture.set_billing(false);
    let incomplete = fixture.json(&["cost", "--all", "--no-scan", "--json"]);
    assert_eq!(incomplete["billing"]["recorded_cash_usd"], "75");
    assert!(incomplete["billing"]["actual_billed_usd"].is_null());
    assert_eq!(
        incomplete["billing"]["actual_billing_status"],
        "incomplete_evidence"
    );
    assert_eq!(
        incomplete["combined"]["api_equivalent_usd"]["lower_bound"],
        "65.5"
    );

    fixture.set_billing(true);
    let complete = fixture.json(&["cost", "--all", "--no-scan", "--json"]);
    assert_eq!(complete["billing"]["recorded_cash_usd"], "75");
    assert_eq!(complete["billing"]["actual_billed_usd"], "75");
    assert_eq!(
        complete["billing"]["actual_billing_status"],
        "attested_complete"
    );
    let serialized = serde_json::to_string(&complete).unwrap();
    assert!(!serialized.contains(BILLING_CANARY));
}

#[test]
fn cost_honors_catalog_revision_and_bounded_dimension_overrides() {
    let fixture = CostFixture::new();
    fixture.run(&["scan"]);
    let (mut config, _) = Config::load(Some(&fixture.config)).expect("load override config");
    config.pricing_dimension_overrides = vec![PricingDimensionOverride {
        id: "fable-global-cost-test".to_string(),
        provider: "anthropic".to_string(),
        canonical_model: "claude-fable-5".to_string(),
        dimension: "inference_geo".to_string(),
        value: "global".to_string(),
        effective_from: Utc.with_ymd_and_hms(2026, 7, 10, 0, 0, 0).unwrap(),
        effective_to: Utc.with_ymd_and_hms(2026, 7, 11, 0, 0, 0).unwrap(),
        attested_at: Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap(),
        note: Some("checked bounded routing setting".to_string()),
    }];
    config.save(&fixture.config).expect("save override config");
    let cost = fixture.json(&[
        "--catalog-revision",
        "2026-07-10.3",
        "cost",
        "--all",
        "--model",
        "claude-fable-5",
        "--no-scan",
        "--json",
    ]);
    assert_eq!(cost["catalog"]["status"]["revision"], "2026-07-10.3");
    assert_eq!(cost["combined"]["api_equivalent_usd"]["status"], "exact");
    assert_eq!(cost["combined"]["api_equivalent_usd"]["lower_bound"], "60");
    assert_eq!(cost["combined"]["api_equivalent_usd"]["upper_bound"], "60");
    assert!(
        cost["combined"]["api_equivalent_usd"]["evidence"]["dimension_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["provenance"] == "user_attested_override")
    );
}

#[test]
fn cost_summarizes_overlapping_provider_evidence_without_changing_local_totals() {
    let fixture = CostFixture::new();
    fixture.run(&["scan"]);
    let before = fixture.json(&["cost", "--all", "--no-scan", "--json"]);
    let import = fixture._temp.path().join("cost-reconciliation.json");
    fs::write(
        &import,
        r#"{
          "schema_version": "token-ledger.reconciliation.v1",
          "buckets": [{
            "bucket_start": "2026-07-10T12:00:00Z",
            "bucket_end": "2026-07-10T13:00:00Z",
            "provider": "openai",
            "model": "gpt-5.6-sol",
            "request_count": 1,
            "input_tokens_uncached": 999,
            "input_tokens_cached": 111,
            "output_tokens": 0
          }]
        }"#,
    )
    .expect("write cost reconciliation evidence");
    let import_text = import.to_string_lossy().to_string();
    fixture.run(&[
        "reconcile",
        "import",
        &import_text,
        "--format",
        "canonical-json",
        "--json",
    ]);
    let after = fixture.json(&["cost", "--all", "--no-scan", "--json"]);
    assert_eq!(after["reconciliation"]["provider_evidence_present"], true);
    assert_eq!(after["reconciliation"]["import_count"], 1);
    assert!(
        after["reconciliation"]["selected_provider_bucket_count"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        after["reconciliation"]["overlapping_evidence_row_count"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_eq!(after["combined"], before["combined"]);
}

#[test]
fn cost_rejects_ambiguous_or_half_open_cli_periods() {
    let fixture = CostFixture::new();
    let ambiguous = fixture
        .command()
        .args(["cost", "--all", "--today", "--json"])
        .output()
        .expect("ambiguous period");
    assert!(!ambiguous.status.success());
    let half = fixture
        .command()
        .args(["cost", "--start", "2026-07-10", "--json"])
        .output()
        .expect("half range");
    assert!(!half.status.success());
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_private(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    assert!(
        !text.contains(PRIVACY_CANARY),
        "private prompt leaked: {text}"
    );
    assert!(
        !text.contains(BILLING_CANARY),
        "billing note leaked: {text}"
    );
    assert!(!text.contains("private-claude-session"));
    assert!(!text.contains("private-codex-session"));
}
