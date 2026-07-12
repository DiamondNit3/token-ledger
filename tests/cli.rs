use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const PRIVACY_CANARY: &str = "TOKEN_LEDGER_PRIVACY_CANARY_91C81F4D";
const LOOKALIKE_SESSION: &str = "tlses_0123456789abcdef01234567";
const LOOKALIKE_MESSAGE: &str = "tlmsg_0123456789abcdef01234567";
const LOOKALIKE_REQUEST: &str = "tlreq_0123456789abcdef01234567";

struct CliFixture {
    _temp: TempDir,
    config: PathBuf,
    database: PathBuf,
    claude_root: PathBuf,
    codex_home: PathBuf,
}

impl CliFixture {
    fn create() -> Self {
        let temp = tempfile::tempdir().expect("create isolated CLI fixture");
        let config = temp.path().join("custom-config").join("ledger.toml");
        let database = temp.path().join("custom-data").join("usage.sqlite3");
        let claude_root = temp.path().join("claude-root");
        let codex_home = temp.path().join("codex-home");

        let claude_session = claude_root
            .join("projects")
            .join("project-a")
            .join("session-claude.jsonl");
        let codex_rollout = codex_home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("10")
            .join("rollout-2026-07-10T14-00-00-11111111-1111-7111-8111-111111111111.jsonl");
        copy_fixture("claude_sessions.jsonl", &claude_session);
        copy_fixture("codex_rollout.jsonl", &codex_rollout);

        Self {
            _temp: temp,
            config,
            database,
            claude_root,
            codex_home,
        }
    }

    fn binary(&self) -> Command {
        let mut command = Command::cargo_bin("token-ledger").expect("compiled token-ledger binary");
        command.arg("--config").arg(&self.config);
        command
    }

    fn initialize(&self) -> Output {
        let mut command = self.binary();
        let output = command
            .arg("--db")
            .arg(&self.database)
            .args(["init", "--tz", "America/New_York"])
            .output()
            .expect("run ledger init");
        assert_success(&output, "ledger init");

        // Exercise source-root overrides through the user-facing TOML config,
        // without importing the application's Config type into this black-box test.
        let raw = fs::read_to_string(&self.config).expect("read generated config");
        let mut config: toml::Value = toml::from_str(&raw).expect("parse generated config");
        let table = config.as_table_mut().expect("generated config is a table");
        table.insert(
            "claude_root".to_owned(),
            toml::Value::String(path_text(&self.claude_root)),
        );
        table.insert(
            "codex_home".to_owned(),
            toml::Value::String(path_text(&self.codex_home)),
        );
        fs::write(
            &self.config,
            toml::to_string_pretty(&config).expect("serialize test config"),
        )
        .expect("write test config overrides");
        output
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self
            .binary()
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run ledger {args:?}: {error}"));
        assert_success(&output, &format!("ledger {}", args.join(" ")));
        output
    }
}

#[test]
fn init_honors_explicit_config_database_and_timezone() {
    let fixture = CliFixture::create();
    let output = fixture.initialize();
    let stdout = output_text(&output.stdout);

    assert!(fixture.config.is_file(), "custom config was not created");
    assert!(
        fixture.database.is_file(),
        "custom database was not created"
    );
    assert!(stdout.contains(&path_text(&fixture.config)));
    assert!(stdout.contains(&path_text(&fixture.database)));
    assert!(stdout.contains("Timezone: America/New_York"));

    let raw = fs::read_to_string(&fixture.config).expect("read custom config");
    let config: toml::Value = toml::from_str(&raw).expect("parse custom config");
    assert_eq!(config["timezone"].as_str(), Some("America/New_York"));
    assert_eq!(
        config["database_path"].as_str(),
        Some(path_text(&fixture.database).as_str())
    );

    let duplicate = fixture
        .binary()
        .arg("init")
        .output()
        .expect("run duplicate init");
    assert!(!duplicate.status.success());
    assert!(output_text(&duplicate.stderr).contains("already exists"));
}

#[test]
fn source_root_precedence_is_cli_then_environment_then_config() {
    let fixture = CliFixture::create();
    fixture.initialize();
    let environment_claude = fixture._temp.path().join("environment-claude");
    let environment_codex = fixture._temp.path().join("environment-codex");
    fs::create_dir_all(&environment_claude).expect("environment Claude root");
    fs::create_dir_all(&environment_codex).expect("environment Codex home");

    let environment = fixture
        .binary()
        .env("CLAUDE_CONFIG_DIR", &environment_claude)
        .env("CODEX_HOME", &environment_codex)
        .arg("--details")
        .arg("doctor")
        .output()
        .expect("environment precedence doctor");
    assert_success(&environment, "environment precedence doctor");
    let environment_stdout = output_text(&environment.stdout);
    assert!(environment_stdout.contains("Claude root (environment)"));
    assert!(environment_stdout.contains("Codex home (environment)"));
    assert!(environment_stdout.contains("Claude Code"));
    assert!(environment_stdout.contains("OpenAI Codex"));
    assert!(environment_stdout.contains("Readable local files"));

    let cli = fixture
        .binary()
        .env("CLAUDE_CONFIG_DIR", &environment_claude)
        .env("CODEX_HOME", &environment_codex)
        .arg("--claude-root")
        .arg(&fixture.claude_root)
        .arg("--codex-home")
        .arg(&fixture.codex_home)
        .arg("--details")
        .arg("doctor")
        .output()
        .expect("CLI precedence doctor");
    assert_success(&cli, "CLI precedence doctor");
    let cli_stdout = output_text(&cli.stdout);
    assert!(cli_stdout.contains("Claude root (CLI override)"));
    assert!(cli_stdout.contains("Codex home (CLI override)"));
    assert!(cli_stdout.contains("Claude Code"));
    assert!(cli_stdout.contains("OpenAI Codex"));
    assert!(cli_stdout.contains("Readable local files"));
}

#[test]
fn scans_both_clients_idempotently_and_exposes_private_safe_reports() {
    let fixture = CliFixture::create();
    fixture.initialize();

    let first_scan = fixture.run(&["scan"]);
    let first_stdout = output_text(&first_scan.stdout);
    assert!(first_stdout.contains("2 sources"), "{first_stdout}");
    assert!(first_stdout.contains("2 scanned"), "{first_stdout}");
    assert!(first_stdout.contains("3 observations"), "{first_stdout}");
    assert_output_private(&first_scan);

    assert_eq!(database_count(&fixture.database, "source_files"), 2);
    assert_eq!(database_count(&fixture.database, "usage_observations"), 3);
    assert_eq!(canonical_event_count(&fixture.database), 3);

    let second_scan = fixture.run(&["scan"]);
    let second_stdout = output_text(&second_scan.stdout);
    assert!(second_stdout.contains("0 scanned"), "{second_stdout}");
    assert!(second_stdout.contains("2 unchanged"), "{second_stdout}");
    assert!(second_stdout.contains("0 observations"), "{second_stdout}");
    assert_eq!(database_count(&fixture.database, "usage_observations"), 3);
    assert_eq!(canonical_event_count(&fixture.database), 3);
    assert_output_private(&second_scan);

    let day = fixture.run(&[
        "day",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    assert_output_private(&day);
    let (day_document, day_rows) = report_rows(&day);
    assert_eq!(day_rows.len(), 2, "unexpected day report: {day_rows:#?}");
    assert_eq!(day_document["schema_version"], "token-ledger.report.v2");
    assert_eq!(day_document["query"]["timezone"], "America/New_York");
    assert_eq!(day_document["query"]["requested_start_date"], "2026-07-10");
    assert_eq!(day_document["catalog"]["revision"], "2026-07-10.3");
    assert!(day_document["catalog"]["sha256"].as_str().is_some());
    assert_eq!(
        day_document["query_coverage"]
            .as_array()
            .expect("query coverage array")
            .len(),
        2
    );

    let claude = row_for_model(&day_rows, "claude-sonnet-4-6");
    assert_eq!(claude["day"], "2026-07-10");
    assert_eq!(claude["client"], "claude");
    assert_eq!(claude["requests"], 1);
    assert_eq!(claude["sessions"], 1);
    assert_eq!(claude["input_tokens_total"], 170);
    assert_eq!(claude["input_tokens_uncached"], 100);
    assert_eq!(claude["input_tokens_cached"], 40);
    assert_eq!(claude["cache_write_5m_tokens"], 20);
    assert_eq!(claude["cache_write_1h_tokens"], 10);
    assert_eq!(claude["output_tokens_total"], 25);
    assert_eq!(claude["web_search_requests"], 1);
    assert_eq!(
        claude["event_ids"]
            .as_array()
            .expect("Claude event IDs")
            .len(),
        1
    );
    assert_nonzero_price(claude);

    let codex = row_for_model(&day_rows, "gpt-5.4");
    assert_eq!(codex["day"], "2026-07-10");
    assert_eq!(codex["client"], "codex");
    assert_eq!(codex["requests"], 1);
    assert_eq!(codex["sessions"], 1);
    assert_eq!(codex["input_tokens_total"], 200);
    assert_eq!(codex["input_tokens_uncached"], 150);
    assert_eq!(codex["input_tokens_cached"], 50);
    assert_eq!(codex["output_tokens_total"], 20);
    assert_eq!(codex["reasoning_output_tokens"], 8);
    assert_nonzero_price(codex);

    let empty_day = fixture.run(&[
        "day",
        "2024-01-01",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    let (empty_document, empty_rows) = report_rows(&empty_day);
    assert!(empty_rows.is_empty());
    assert!(
        empty_document["query_coverage"]
            .as_array()
            .expect("empty-day coverage")
            .iter()
            .all(|entry| entry["status"] == "outside_observed_window")
    );
    let empty_human = fixture.run(&["day", "2024-01-01", "--tz", "America/New_York", "--no-scan"]);
    assert!(output_text(&empty_human.stdout).contains("not automatically a verified zero"));

    let range = fixture.run(&[
        "range",
        "2026-07-10",
        "2026-07-11",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    assert_output_private(&range);
    let (range_document, range_rows) = report_rows(&range);
    assert_eq!(range_rows.len(), 3, "unexpected range: {range_rows:#?}");
    assert_eq!(
        range_document["query"]["requested_end_date_inclusive"],
        "2026-07-11"
    );
    assert_eq!(
        row_for_model(&range_rows, "claude-private-unpriced-model-91c81f4d")["day"],
        "2026-07-11"
    );

    let csv_path = fixture._temp.path().join("auditable-report.csv");
    let csv_path_text = path_text(&csv_path);
    let export = fixture.run(&[
        "export",
        "--start",
        "2026-07-10",
        "--end",
        "2026-07-11",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--format",
        "csv",
        "--output",
        &csv_path_text,
    ]);
    assert_output_private(&export);
    let mut csv = csv::Reader::from_path(&csv_path).expect("open report CSV");
    let headers = csv.headers().expect("CSV headers").clone();
    let records = csv
        .records()
        .collect::<Result<Vec<_>, _>>()
        .expect("CSV records");
    assert_eq!(records[0].get(0), Some("metadata"));
    assert!(
        records
            .iter()
            .skip(1)
            .all(|record| record.get(0) == Some("data"))
    );
    let coverage_index = headers
        .iter()
        .position(|header| header == "coverage_json")
        .expect("coverage_json header");
    let coverage: JsonValue =
        serde_json::from_str(&records[0][coverage_index]).expect("coverage metadata JSON");
    assert_eq!(
        coverage["query_coverage"]
            .as_array()
            .expect("CSV query coverage")
            .len(),
        2
    );
    assert!(headers.iter().any(|header| header == "event_ids_json"));

    let models = fixture.run(&["models", "--json"]);
    assert_output_private(&models);
    let model_rows = json_array(&models);
    assert_eq!(model_rows.len(), 3);
    assert!(model_rows.iter().any(|row| {
        row["client"] == "claude_code" && row["model"] == "claude-sonnet-4-6" && row["events"] == 1
    }));
    assert!(model_rows.iter().any(|row| {
        row["client"] == "openai_codex" && row["model"] == "gpt-5.4" && row["events"] == 1
    }));

    let sessions = fixture.run(&[
        "sessions",
        "--date",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    assert_output_private(&sessions);
    let session_rows = json_array(&sessions);
    assert_eq!(session_rows.len(), 2);
    assert!(session_rows.iter().any(|row| {
        row["client"] == "claude_code"
            && row["requests"] == 1
            && row["input_tokens"] == 170
            && row["output_tokens"] == 25
    }));
    assert!(session_rows.iter().any(|row| {
        row["client"] == "openai_codex"
            && row["requests"] == 1
            && row["input_tokens"] == 200
            && row["output_tokens"] == 20
    }));
    let sessions_stdout = output_text(&sessions.stdout);
    assert!(!sessions_stdout.contains("claude-session-known"));
    assert!(!sessions_stdout.contains("codex-session-known"));

    let event_id = stable_event_id("claude_code", "message:msg-claude-known");
    let explain = fixture.run(&["explain", "--event", &event_id, "--json"]);
    assert_output_private(&explain);
    let explained: JsonValue = serde_json::from_slice(&explain.stdout).expect("parse explain JSON");
    assert_eq!(explained["event"]["event_id"], event_id);
    assert_eq!(explained["event"]["raw_model"], "claude-sonnet-4-6");
    assert_eq!(explained["event"]["usage"]["output_tokens_total"], 25);
    assert_eq!(explained["estimate"]["status"], "priced");
    assert_eq!(explained["provenance"]["event_id"], event_id);
    assert_eq!(
        explained["provenance"]["observations"]
            .as_array()
            .expect("provenance observations")
            .len(),
        1
    );
    assert!(
        explained["estimate"]["pricing_evidence"]
            .as_array()
            .expect("pricing evidence")
            .iter()
            .any(|record| record["record_type"] == "rate")
    );

    assert_database_private(&fixture.database);
}

#[test]
fn persisted_identifiers_remain_private_even_when_raw_display_is_requested() {
    let fixture = CliFixture::create();
    fixture.initialize();
    fixture.run(&["scan"]);

    let raw = fs::read_to_string(&fixture.config).expect("read generated config");
    let mut config: toml::Value = toml::from_str(&raw).expect("parse generated config");
    config
        .as_table_mut()
        .expect("generated config is a table")
        .insert("show_raw_ids".to_owned(), toml::Value::Boolean(true));
    fs::write(
        &fixture.config,
        toml::to_string_pretty(&config).expect("serialize raw-id config"),
    )
    .expect("write raw-id config");

    let sessions = fixture.run(&[
        "sessions",
        "--date",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    let day = fixture.run(&[
        "day",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    let document = json_value(&day);
    let event_ids = document["rows"]
        .as_array()
        .expect("report rows")
        .iter()
        .flat_map(|row| row["event_ids"].as_array().into_iter().flatten())
        .filter_map(JsonValue::as_str)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert!(!event_ids.is_empty());

    let mut output = output_text(&sessions.stdout);
    output.push_str(&output_text(&day.stdout));
    for event_id in event_ids {
        let explain = fixture.run(&["explain", "--event", &event_id, "--json"]);
        output.push_str(&output_text(&explain.stdout));
    }
    for raw_identifier in [
        "claude-session-known",
        "claude-request-known",
        "msg-claude-known",
        "codex-session-known",
        "11111111-1111-7111-8111-111111111111",
    ] {
        assert!(
            !output.contains(raw_identifier),
            "persisted raw identifier leaked despite the storage boundary: {raw_identifier}"
        );
    }
    assert!(output.contains("tlses_") || output.contains("evt_"));
}

#[test]
fn pseudonym_shaped_provider_ids_are_transformed_in_db_and_cli_output() {
    let fixture = CliFixture::create();
    fixture.initialize();
    let source = fixture
        .claude_root
        .join("projects")
        .join("lookalike-project")
        .join("lookalike.jsonl");
    fs::create_dir_all(source.parent().expect("lookalike source parent"))
        .expect("create lookalike source parent");
    let record = serde_json::json!({
        "type": "assistant",
        "timestamp": "2026-07-10T16:00:00Z",
        "sessionId": LOOKALIKE_SESSION,
        "requestId": LOOKALIKE_REQUEST,
        "message": {
            "id": LOOKALIKE_MESSAGE,
            "model": "claude-sonnet-4-6",
            "usage": {
                "input_tokens": 11,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
                "output_tokens": 2
            }
        }
    });
    fs::write(&source, format!("{record}\n")).expect("write lookalike source");
    fixture.run(&["scan"]);

    let raw = fs::read_to_string(&fixture.config).expect("read generated config");
    let mut config: toml::Value = toml::from_str(&raw).expect("parse generated config");
    config
        .as_table_mut()
        .expect("generated config is a table")
        .insert("show_raw_ids".to_owned(), toml::Value::Boolean(true));
    fs::write(
        &fixture.config,
        toml::to_string_pretty(&config).expect("serialize raw-id config"),
    )
    .expect("write raw-id config");

    let sessions = fixture.run(&[
        "sessions",
        "--date",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    let day = fixture.run(&[
        "day",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    let event_id = stable_event_id("claude_code", &format!("message:{LOOKALIKE_MESSAGE}"));
    let explain = fixture.run(&["explain", "--event", &event_id, "--json"]);
    let mut rendered = output_text(&sessions.stdout);
    rendered.push_str(&output_text(&day.stdout));
    rendered.push_str(&output_text(&explain.stdout));
    rendered.push_str(&output_text(&sessions.stderr));
    rendered.push_str(&output_text(&day.stderr));
    rendered.push_str(&output_text(&explain.stderr));
    for raw_identifier in [LOOKALIKE_SESSION, LOOKALIKE_MESSAGE, LOOKALIKE_REQUEST] {
        assert!(
            !rendered.contains(raw_identifier),
            "pseudonym-shaped provider identifier leaked to CLI output: {raw_identifier}"
        );
    }

    let connection = Connection::open(&fixture.database).expect("open test ledger");
    let stored: (String, String, String) = connection
        .query_row(
            r#"SELECT session_id, provider_message_id, dimensions_json
               FROM usage_observations
               WHERE occurred_at_utc LIKE '2026-07-10T16:00:00%'"#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read lookalike observation");
    assert_ne!(stored.0, LOOKALIKE_SESSION);
    assert_ne!(stored.1, LOOKALIKE_MESSAGE);
    let dimensions: JsonValue =
        serde_json::from_str(&stored.2).expect("parse stored pricing dimensions");
    assert_ne!(
        dimensions["provider_request_id"].as_str(),
        Some(LOOKALIKE_REQUEST)
    );
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint lookalike ledger");
    drop(connection);
    assert_database_excludes(
        &fixture.database,
        &[LOOKALIKE_SESSION, LOOKALIKE_MESSAGE, LOOKALIKE_REQUEST],
    );
}

#[test]
fn codex_session_identity_survives_persisted_incremental_state() {
    let fixture = CliFixture::create();
    fixture.initialize();
    fixture.run(&["scan"]);
    let rollout = fixture
        .codex_home
        .join("sessions")
        .join("2026")
        .join("07")
        .join("10")
        .join("rollout-2026-07-10T14-00-00-11111111-1111-7111-8111-111111111111.jsonl");
    let mut content = fs::read_to_string(&rollout).expect("read Codex rollout");
    content.push_str(concat!(
        "{\"timestamp\":\"2026-07-10T14:00:07Z\",\"type\":\"event_msg\",",
        "\"payload\":{\"type\":\"token_count\",\"info\":{",
        "\"total_token_usage\":{\"input_tokens\":250,\"cached_input_tokens\":60,",
        "\"cache_write_tokens\":0,\"output_tokens\":25,\"reasoning_output_tokens\":10,",
        "\"total_tokens\":275},\"last_token_usage\":{\"input_tokens\":50,",
        "\"cached_input_tokens\":10,\"cache_write_tokens\":0,\"output_tokens\":5,",
        "\"reasoning_output_tokens\":2,\"total_tokens\":55},",
        "\"model_context_window\":200000}}}\n"
    ));
    fs::write(&rollout, content).expect("append Codex usage boundary");
    fixture.run(&["scan"]);

    let connection = Connection::open(&fixture.database).expect("open test ledger");
    let (sessions, events): (i64, i64) = connection
        .query_row(
            r#"SELECT COUNT(DISTINCT session_id), COUNT(DISTINCT event_key)
               FROM usage_observations WHERE client='openai_codex'"#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read incremental Codex identities");
    assert_eq!(
        sessions, 1,
        "persisted state changed Codex session identity"
    );
    assert_eq!(events, 2, "new cumulative boundary was not retained once");
    let state: String = connection
        .query_row(
            r#"SELECT adapter_state FROM source_files WHERE client='openai_codex'"#,
            [],
            |row| row.get(0),
        )
        .expect("read persisted Codex state");
    let state: JsonValue = serde_json::from_str(&state).expect("parse persisted Codex state");
    assert_eq!(state["session_ids_private"], true);
    assert!(!state.to_string().contains("codex-session-known"));
}

#[test]
fn migrated_codex_fork_identity_survives_single_source_rebuild() {
    const RAW_SESSION: &str = "codex-session-known";
    const PARENT_THREAD: &str = "11111111-1111-7111-8111-111111111111";
    const CHILD_THREAD: &str = "22222222-2222-7222-8222-222222222222";
    const BOUNDARY: &str = "i=200;ci=50;o=20;ro=8;t=220;w5=0;w1=0;wu=0";

    let fixture = CliFixture::create();
    fixture.initialize();
    let parent = fixture
        .codex_home
        .join("sessions/2026/07/10")
        .join(format!("rollout-2026-07-10T14-00-00-{PARENT_THREAD}.jsonl"));
    let child = fixture
        .codex_home
        .join("sessions/2026/07/10")
        .join(format!("rollout-2026-07-10T14-00-00-{CHILD_THREAD}.jsonl"));
    let child_content = fs::read_to_string(&parent)
        .expect("read parent Codex rollout")
        .replace(PARENT_THREAD, CHILD_THREAD);
    fs::write(&child, child_content).expect("write copied child Codex rollout");
    fixture.run(&["scan"]);

    let legacy_event_key = stable_id_parts(&["codex-counter-boundary", RAW_SESSION, "0", BOUNDARY]);
    let legacy_state = serde_json::json!({
        "canonical_meta_seen": true,
        "logical_session_id": RAW_SESSION,
        "physical_thread_id": PARENT_THREAD,
        "client_version": "0.144.0",
        "model": "gpt-5.4",
        "provider": "openai",
        "epoch": 0,
        "previous": {
            "input": 200,
            "cached_input": 50,
            "output": 20,
            "reasoning_output": 8,
            "total": 220,
            "cache_write_5m": 0,
            "cache_write_1h": 0,
            "cache_write_unknown": 0,
            "cached_input_reported": true,
            "reasoning_output_reported": true,
            "cache_write_reported": true
        }
    });
    {
        let connection = Connection::open(&fixture.database).expect("open pre-migration ledger");
        connection
            .execute_batch(
                "DROP TRIGGER IF EXISTS guard_schema_version_no_downgrade;
                 DROP TRIGGER IF EXISTS guard_schema_version_no_delete;
                 DROP TRIGGER IF EXISTS guard_schema_version_no_replace;",
            )
            .expect("emulate pre-v7 schema guards");
        connection
            .execute("UPDATE meta SET value='4' WHERE key='schema_version'", [])
            .expect("mark ledger schema v4");
        connection
            .execute(
                r#"UPDATE usage_observations
                   SET event_key=?1, session_id=?2
                   WHERE client='openai_codex'"#,
                rusqlite::params![legacy_event_key, RAW_SESSION],
            )
            .expect("restore v4 Codex observation identity");
        connection
            .execute(
                r#"UPDATE source_files SET adapter_state=?1
                   WHERE client='openai_codex'"#,
                [legacy_state.to_string()],
            )
            .expect("restore v4 Codex parser state");
    }
    fs::remove_file(&child).expect("remove copied source before source-local rebuild");

    fixture.run(&["scan", "--client", "codex", "--rebuild"]);
    let connection = Connection::open(&fixture.database).expect("open migrated ledger");
    let (schema, observations, canonical_events, aliases): (i64, i64, i64, i64) = connection
        .query_row(
            r#"SELECT
                   (SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'),
                   (SELECT COUNT(*) FROM usage_observations WHERE client='openai_codex'),
                   (SELECT COUNT(*) FROM (
                       SELECT event_key FROM usage_observations
                       WHERE client='openai_codex' GROUP BY event_key
                   )),
                   (SELECT COUNT(*) FROM codex_event_identity_aliases)"#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read migrated fork identity counts");
    assert_eq!(schema, 7);
    assert_eq!(observations, 2);
    assert_eq!(canonical_events, 1, "copied boundary was double-counted");
    assert_eq!(
        aliases, 6,
        "each migrated observation has exactly three bounded private scope candidates"
    );
    let alias_rows = {
        let mut statement = connection
            .prepare(
                r#"SELECT canonical_event_key, source_locator, session_scope,
                          usage_event_index
                   FROM codex_event_identity_aliases"#,
            )
            .expect("prepare alias privacy query");
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .expect("query alias privacy rows")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect alias privacy rows")
    };
    assert!(alias_rows.iter().all(|(event, locator, scope, ordinal)| {
        event.starts_with("evt_")
            && locator.starts_with("line ")
            && scope.starts_with("tlasp_")
            && *ordinal > 0
            && !locator.contains(RAW_SESSION)
    }));
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint migrated fork ledger");
    drop(connection);

    let report = fixture.run(&[
        "day",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--client",
        "codex",
        "--no-scan",
        "--json",
    ]);
    let rows = json_value(&report)["rows"]
        .as_array()
        .expect("Codex report rows")
        .clone();
    let codex = row_for_model(&rows, "gpt-5.4");
    assert_eq!(codex["input_tokens_total"], 200);
    assert_eq!(codex["output_tokens_total"], 20);
    assert_database_excludes(
        &fixture.database,
        &[RAW_SESSION, PARENT_THREAD, CHILD_THREAD],
    );
}

#[test]
fn shifted_codex_copy_after_migration_is_provisional_without_duplicate_usage() {
    const RAW_SESSION: &str = "codex-session-known";
    const PARENT_THREAD: &str = "11111111-1111-7111-8111-111111111111";
    const CHILD_THREAD: &str = "33333333-3333-7333-8333-333333333333";
    const BOUNDARY: &str = "i=200;ci=50;o=20;ro=8;t=220;w5=0;w1=0;wu=0";

    let fixture = CliFixture::create();
    fixture.initialize();
    fixture.run(&["scan", "--client", "codex"]);
    let parent = fixture
        .codex_home
        .join("sessions/2026/07/10")
        .join(format!("rollout-2026-07-10T14-00-00-{PARENT_THREAD}.jsonl"));
    let child = fixture
        .codex_home
        .join("sessions/2026/07/10")
        .join(format!("rollout-2026-07-10T14-00-00-{CHILD_THREAD}.jsonl"));
    let legacy_event_key = stable_id_parts(&["codex-counter-boundary", RAW_SESSION, "0", BOUNDARY]);
    let legacy_state = serde_json::json!({
        "canonical_meta_seen": true,
        "logical_session_id": RAW_SESSION,
        "physical_thread_id": PARENT_THREAD,
        "epoch": 0,
        "previous": {
            "input": 200,
            "cached_input": 50,
            "output": 20,
            "reasoning_output": 8,
            "total": 220,
            "cache_write_5m": 0,
            "cache_write_1h": 0,
            "cache_write_unknown": 0,
            "cached_input_reported": true,
            "reasoning_output_reported": true,
            "cache_write_reported": true
        }
    });
    {
        let connection = Connection::open(&fixture.database).expect("open v4 source ledger");
        connection
            .execute_batch(
                "DROP TRIGGER IF EXISTS guard_schema_version_no_downgrade;
                 DROP TRIGGER IF EXISTS guard_schema_version_no_delete;
                 DROP TRIGGER IF EXISTS guard_schema_version_no_replace;",
            )
            .expect("emulate pre-v7 schema guards");
        connection
            .execute("UPDATE meta SET value='4' WHERE key='schema_version'", [])
            .expect("mark v4 ledger");
        connection
            .execute(
                r#"UPDATE usage_observations SET event_key=?1, session_id=?2
                   WHERE client='openai_codex'"#,
                rusqlite::params![legacy_event_key, RAW_SESSION],
            )
            .expect("restore legacy event identity");
        connection
            .execute(
                r#"UPDATE source_files SET adapter_state=?1
                   WHERE client='openai_codex'"#,
                [legacy_state.to_string()],
            )
            .expect("restore legacy state");
    }
    fixture.run(&["scan", "--client", "codex", "--rebuild"]);

    // The copy appears only after migration. Prefixing an accounting-irrelevant
    // blank line shifts its immutable locator, so it cannot be globally
    // anchored. Its matching private scope+ordinal must fail closed.
    let shifted = format!(
        "\n{}",
        fs::read_to_string(&parent)
            .expect("read migrated parent rollout")
            .replace(PARENT_THREAD, CHILD_THREAD)
    );
    fs::write(&child, shifted).expect("write shifted post-migration copy");
    fixture.run(&["scan", "--client", "codex"]);

    let connection = Connection::open(&fixture.database).expect("open shifted-copy ledger");
    let (observations, canonical_events, child_observations, status, provisional): (
        i64,
        i64,
        i64,
        String,
        i64,
    ) = connection
        .query_row(
            r#"SELECT
                   (SELECT COUNT(*) FROM usage_observations WHERE client='openai_codex'),
                   (SELECT COUNT(*) FROM (
                       SELECT event_key FROM usage_observations
                       WHERE client='openai_codex' GROUP BY event_key
                   )),
                   (SELECT COUNT(*) FROM usage_observations o
                    JOIN source_files s ON s.id=o.source_file_id
                    WHERE s.path=(SELECT path FROM source_files
                                  WHERE client='openai_codex'
                                  ORDER BY id DESC LIMIT 1)),
                   (SELECT status FROM scan_runs ORDER BY id DESC LIMIT 1),
                   (SELECT provisional FROM scan_runs ORDER BY id DESC LIMIT 1)"#,
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("read shifted-copy scan outcome");
    assert_eq!(observations, 1);
    assert_eq!(canonical_events, 1);
    assert_eq!(child_observations, 0);
    assert_eq!(status, "partial");
    assert_eq!(provisional, 1);
    let (warning_code, warning_message): (String, String) = connection
        .query_row(
            r#"SELECT code, message FROM scan_warnings
               ORDER BY id DESC LIMIT 1"#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read sanitized shifted-copy warning");
    assert_eq!(warning_code, "source_scan_failed");
    assert_eq!(
        warning_message,
        "warning details redacted at the storage boundary"
    );
}

#[test]
fn unknown_model_cost_is_null_and_explicitly_unpriced_never_zero() {
    let fixture = CliFixture::create();
    fixture.initialize();
    fixture.run(&["scan"]);

    let day = fixture.run(&[
        "day",
        "2026-07-11",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    assert_output_private(&day);
    let (document, rows) = report_rows(&day);
    assert_eq!(rows.len(), 1);
    assert_eq!(document["schema_version"], "token-ledger.report.v2");
    let row = &rows[0];
    assert_eq!(row["model"], "claude-private-unpriced-model-91c81f4d");
    assert_eq!(row["unpriced_events"], 1);
    assert_eq!(row["priced_events"], 0);
    assert!(row["api_equivalent_usd"].is_null());
    assert!(row["known_api_equivalent_usd"].is_null());
    assert_ne!(row["api_equivalent_usd"], "0");

    let human = fixture.run(&["day", "2026-07-11", "--tz", "America/New_York", "--no-scan"]);
    let human_stdout = output_text(&human.stdout);
    assert!(human_stdout.contains("Unpriced events"), "{human_stdout}");
    assert!(
        human_stdout.contains("never treated as $0"),
        "{human_stdout}"
    );

    let event_id = stable_event_id("claude_code", "message:msg-claude-unknown");
    let explain = fixture.run(&["explain", "--event", &event_id, "--json"]);
    let explained: JsonValue = serde_json::from_slice(&explain.stdout).expect("parse explain JSON");
    assert_eq!(explained["estimate"]["status"], "unpriced");
    assert!(explained["estimate"]["api_equivalent_usd"].is_null());
    assert!(explained["estimate"]["known_api_equivalent_usd"].is_null());
    assert_output_private(&explain);
    assert_database_private(&fixture.database);
}

#[test]
fn v02_reports_filter_render_html_and_offer_human_drilldown() {
    let fixture = CliFixture::create();
    fixture.initialize();
    fixture.run(&["scan"]);

    let filtered = fixture.run(&[
        "day",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--client",
        "claude",
        "--client",
        "codex",
        "--model",
        "claude-sonnet-4-6",
        "--model",
        "gpt-5.4",
        "--json",
    ]);
    let (document, rows) = report_rows(&filtered);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        document["query"]["client_filters"]
            .as_array()
            .expect("client filters")
            .len(),
        2
    );
    assert_eq!(
        document["query"]["model_filters"]
            .as_array()
            .expect("model filters")
            .len(),
        2
    );

    let human = fixture.run(&[
        "--details",
        "day",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--no-scan",
    ]);
    let human_text = output_text(&human.stdout);
    let summary_at = human_text
        .find("API-equivalent list-price value")
        .expect("human summary");
    let table_at = human_text.find("BY MODEL").expect("human table");
    assert!(summary_at < table_at, "summary should precede table");
    assert!(human_text.contains("token-ledger explain --event evt_"));
    assert!(human_text.contains("Snapshot "));

    let event_id = stable_event_id("claude_code", "message:msg-claude-known");
    let drilldown = fixture.run(&["explain", "--event", &event_id]);
    let drilldown_text = output_text(&drilldown.stdout);
    assert!(drilldown_text.starts_with("TOKEN LEDGER / EVENT EXPLAIN"));
    assert!(drilldown_text.contains("API equivalent"));
    assert!(drilldown_text.contains("PROVENANCE"));
    assert_output_private(&drilldown);

    let html_path = fixture._temp.path().join("share-safe-report.html");
    let html_path_text = path_text(&html_path);
    let html_export = fixture.run(&[
        "day",
        "2026-07-10",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--html",
        &html_path_text,
    ]);
    assert!(output_text(&html_export.stdout).contains("HTML REPORT WRITTEN"));
    let html = fs::read_to_string(&html_path).expect("read HTML report");
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("@media print"));
    assert!(html.contains("SHARE-SAFE DEFAULT"));
    assert!(html.contains("Usage register"));
    assert!(!html.contains("evt_"), "HTML must omit canonical event IDs");
    assert!(!html.contains("claude-session-known"));
    assert!(!html.contains("codex-session-known"));
    assert!(!html.contains(PRIVACY_CANARY));
    assert!(!html.contains("<script"));

    let status = fixture.run(&["prices", "status"]);
    let status_text = output_text(&status.stdout);
    assert!(status_text.starts_with("TOKEN LEDGER / PRICE CATALOG"));
    assert!(!status_text.contains("CatalogStatus {"));
    let status_json = fixture.run(&["prices", "status", "--json"]);
    assert!(json_value(&status_json)["revision"].as_str().is_some());
    let verify = fixture.run(&["prices", "verify"]);
    assert!(output_text(&verify.stdout).contains("[PASSED]"));
}

#[test]
fn today_command_and_natural_date_keywords_emit_stable_json() {
    let fixture = CliFixture::create();
    fixture.initialize();

    let today = fixture.run(&["today", "--tz", "America/New_York", "--no-scan", "--json"]);
    let today_json = json_value(&today);
    let expected_today = chrono::Utc::now()
        .with_timezone(&chrono_tz::America::New_York)
        .date_naive()
        .to_string();
    assert_eq!(today_json["query"]["requested_start_date"], expected_today);

    let yesterday = fixture.run(&[
        "day",
        "yesterday",
        "--tz",
        "America/New_York",
        "--no-scan",
        "--json",
    ]);
    let yesterday_json = json_value(&yesterday);
    assert_ne!(
        yesterday_json["query"]["requested_start_date"],
        today_json["query"]["requested_start_date"]
    );
}

#[test]
fn audit_console_adapts_width_color_plain_and_machine_modes() {
    let fixture = CliFixture::create();
    fixture.initialize();
    fixture.run(&["scan"]);

    for (width, expected, absent) in [
        ("50", "  Client:", "MODEL                 "),
        ("80", " requests | ", "MODEL                 "),
        ("120", "MODEL", "  Client:"),
    ] {
        let output = fixture
            .binary()
            .env("TOKEN_LEDGER_WIDTH", width)
            .args([
                "--color",
                "never",
                "--unicode",
                "never",
                "cost",
                "--all",
                "--no-scan",
            ])
            .output()
            .expect("responsive cost output");
        assert_success(&output, "responsive cost output");
        let text = output_text(&output.stdout);
        assert!(text.contains(expected), "width {width}: {text}");
        assert!(!text.contains(absent), "width {width}: {text}");
        assert!(!text.contains("\u{1b}["), "width {width}: ANSI leaked");
        assert!(
            text.lines()
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0)
                <= width.parse::<usize>().unwrap(),
            "width {width}: line overflow\n{text}"
        );
    }

    let colored = fixture
        .binary()
        .args(["--color", "always", "cost", "--all", "--no-scan"])
        .output()
        .expect("forced color output");
    assert_success(&colored, "forced color output");
    assert!(output_text(&colored.stdout).contains("\u{1b}["));

    let plain = fixture
        .binary()
        .args([
            "--plain",
            "--color",
            "always",
            "--unicode",
            "always",
            "cost",
            "--all",
            "--no-scan",
        ])
        .output()
        .expect("plain output");
    assert_success(&plain, "plain output");
    let plain = output_text(&plain.stdout);
    assert!(!plain.contains("\u{1b}["));
    assert!(!plain.contains('✓'));
    assert!(!plain.contains('─'));
    assert!(plain.contains("TOKEN LEDGER / COST"));

    let json = fixture
        .binary()
        .args([
            "--color",
            "always",
            "day",
            "2026-07-10",
            "--tz",
            "America/New_York",
            "--no-scan",
            "--json",
        ])
        .output()
        .expect("colored JSON request");
    assert_success(&json, "colored JSON request");
    assert!(!output_text(&json.stdout).contains("\u{1b}["));
    assert_eq!(
        json_value(&json)["schema_version"],
        "token-ledger.report.v2"
    );

    let plain_help = fixture
        .binary()
        .args(["--color", "never", "--help"])
        .output()
        .expect("plain help");
    assert_success(&plain_help, "plain help");
    assert!(!output_text(&plain_help.stdout).contains("\u{1b}["));
    let colored_help = fixture
        .binary()
        .args(["--color", "always", "--help"])
        .output()
        .expect("colored help");
    assert_success(&colored_help, "colored help");
    assert!(output_text(&colored_help.stdout).contains("\u{1b}["));

    let welcome = fixture.binary().output().expect("welcome output");
    assert_success(&welcome, "welcome output");
    let welcome = output_text(&welcome.stdout);
    assert!(welcome.contains("TOKEN LEDGER"));
    assert!(welcome.contains("QUICK START"));
}

fn copy_fixture(name: &str, destination: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    fs::create_dir_all(destination.parent().expect("fixture parent"))
        .expect("create fixture destination");
    fs::copy(&source, destination).unwrap_or_else(|error| {
        panic!(
            "copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    });
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output_text(&output.stdout),
        output_text(&output.stderr)
    );
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn path_text(path: &Path) -> String {
    path.as_os_str()
        .to_str()
        .unwrap_or_else(|| panic!("test path is not UTF-8: {}", path.display()))
        .to_owned()
}

fn json_array(output: &Output) -> Vec<JsonValue> {
    let value = json_value(output);
    value
        .as_array()
        .expect("CLI JSON output should be an array")
        .clone()
}

fn json_value(output: &Output) -> JsonValue {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "parse command JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            output_text(&output.stdout),
            output_text(&output.stderr)
        )
    })
}

fn report_rows(output: &Output) -> (JsonValue, Vec<JsonValue>) {
    let document = json_value(output);
    let rows = document["rows"]
        .as_array()
        .expect("report JSON should contain a rows array")
        .clone();
    (document, rows)
}

fn row_for_model<'a>(rows: &'a [JsonValue], model: &str) -> &'a JsonValue {
    rows.iter()
        .find(|row| row["model"] == model)
        .unwrap_or_else(|| panic!("missing model {model} in {rows:#?}"))
}

fn assert_nonzero_price(row: &JsonValue) {
    let amount = row["api_equivalent_usd"]
        .as_str()
        .unwrap_or_else(|| panic!("expected fully priced row: {row:#?}"));
    assert_ne!(amount, "0", "known model unexpectedly priced at zero");
    assert_eq!(row["unpriced_events"], 0);
}

fn database_count(database: &Path, table: &str) -> i64 {
    assert!(["source_files", "usage_observations"].contains(&table));
    let connection = Connection::open(database).expect("open test ledger");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("read ledger count")
}

fn canonical_event_count(database: &Path) -> i64 {
    let connection = Connection::open(database).expect("open test ledger");
    connection
        .query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM usage_observations GROUP BY client, event_key)",
            [],
            |row| row.get(0),
        )
        .expect("read canonical event count")
}

fn stable_event_id(client: &str, event_key: &str) -> String {
    stable_id_parts(&[client, event_key])
}

fn stable_id_parts(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    format!("evt_{}", &digest[..24])
}

fn assert_output_private(output: &Output) {
    let stdout = output_text(&output.stdout);
    let stderr = output_text(&output.stderr);
    assert!(
        !stdout.contains(PRIVACY_CANARY),
        "privacy canary leaked to stdout"
    );
    assert!(
        !stderr.contains(PRIVACY_CANARY),
        "privacy canary leaked to stderr"
    );
}

fn assert_database_private(database: &Path) {
    assert_database_excludes(database, &[PRIVACY_CANARY]);
}

fn assert_database_excludes(database: &Path, markers: &[&str]) {
    let directory = database.parent().expect("database parent");
    let base = database
        .file_name()
        .and_then(OsStr::to_str)
        .expect("database filename");
    for entry in fs::read_dir(directory).expect("read database directory") {
        let entry = entry.expect("database directory entry");
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(base) || !entry.path().is_file() {
            continue;
        }
        let bytes = fs::read(entry.path()).expect("read database family file");
        for marker in markers {
            assert!(
                !bytes
                    .windows(marker.len())
                    .any(|window| window == marker.as_bytes()),
                "private marker {marker:?} leaked into {}",
                entry.path().display()
            );
        }
    }
}
