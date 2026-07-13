use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    config: PathBuf,
    database: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary reconciliation fixture");
        let config = temp.path().join("config").join("ledger.toml");
        let database = temp.path().join("data").join("ledger.sqlite3");
        let fixture = Self {
            _temp: temp,
            config,
            database,
        };
        let output = fixture
            .command()
            .arg("--db")
            .arg(&fixture.database)
            .args(["init", "--tz", "America/New_York"])
            .output()
            .expect("initialize reconciliation fixture");
        assert!(
            output.status.success(),
            "ledger init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("token-ledger").expect("compiled token-ledger");
        command.arg("--config").arg(&self.config);
        command
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.command().args(args).output().expect("run ledger");
        assert!(
            output.status.success(),
            "ledger {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("machine-readable reconciliation JSON")
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn reconcile_import_status_and_report_are_idempotent_and_separate() {
    let fixture = Fixture::new();
    let native = fixture_path("openai_organization_usage.json");
    let native_text = native.to_string_lossy().to_string();
    let raw_digest = hex::encode(Sha256::digest(
        fs::read(&native).expect("read native reconciliation fixture"),
    ));

    let first = fixture.json(&[
        "reconcile",
        "import",
        &native_text,
        "--format",
        "openai",
        "--json",
    ]);
    assert_eq!(first["imported"], true);
    assert_eq!(first["bucket_count"], 1);
    assert_ne!(first["content_digest"], raw_digest);
    let serialized = serde_json::to_string(&first).expect("serialize receipt");
    assert!(!serialized.contains("project-private-canary"));
    assert!(!serialized.contains("key-private-canary"));
    assert!(!serialized.contains(native.file_name().unwrap().to_string_lossy().as_ref()));

    let second = fixture.json(&[
        "reconcile",
        "import",
        &native_text,
        "--format",
        "openai",
        "--json",
    ]);
    assert_eq!(second["imported"], false);

    let status = fixture.json(&["reconcile", "status", "--json"]);
    assert_eq!(status["import_count"], 1);
    assert_eq!(status["bucket_count"], 1);
    assert_eq!(status["providers"][0], "openai");
    assert_ne!(status["latest_imports"][0]["content_digest"], raw_digest);

    let report = fixture.json(&["reconcile", "report", "--no-scan", "--json"]);
    assert_eq!(
        report["schema_version"],
        "token-ledger.reconciliation-report.v1"
    );
    assert_eq!(report["summary"]["provider_only"], 1);
    assert_eq!(report["rows"][0]["classification"], "provider_only");

    let connection = Connection::open(&fixture.database).expect("open reconciliation database");
    let observations: i64 = connection
        .query_row("SELECT COUNT(*) FROM usage_observations", [], |row| {
            row.get(0)
        })
        .expect("count local observations");
    assert_eq!(observations, 0, "provider import synthesized local usage");
}

#[test]
fn malformed_import_does_not_echo_content_or_path() {
    const SECRET: &str = "super-secret-provider-key-canary";
    let fixture = Fixture::new();
    let path = fixture._temp.path().join("private-provider-export.json");
    fs::write(&path, format!("{{not-json:{SECRET}"))
        .expect("write malformed reconciliation fixture");
    let output = fixture
        .command()
        .arg("reconcile")
        .arg("import")
        .arg(&path)
        .arg("--json")
        .output()
        .expect("run malformed reconciliation import");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains(SECRET));
    assert!(!stderr.contains("private-provider-export.json"));
}

#[test]
fn extreme_reconciliation_values_exit_cleanly_without_a_panic() {
    let fixture = Fixture::new();
    let path = fixture._temp.path().join("extreme-values.json");
    let maximum = rust_decimal::Decimal::MAX.to_string();
    let document = serde_json::json!({
        "schema_version": "token-ledger.reconciliation.v1",
        "source_kind": "extreme_cost_fixture",
        "buckets": [
            {"start":"2026-07-10","end":"2026-07-11","provider":"openai","provider_metered_usd":maximum},
            {"start":"2026-07-10","end":"2026-07-11","provider":"openai","provider_metered_usd":maximum}
        ]
    });
    fs::write(
        &path,
        serde_json::to_vec(&document).expect("serialize extreme fixture"),
    )
    .expect("write extreme reconciliation fixture");

    let path_text = path.to_string_lossy().to_string();
    let output = fixture
        .command()
        .args([
            "reconcile",
            "import",
            &path_text,
            "--format",
            "canonical-json",
            "--json",
        ])
        .output()
        .expect("run extreme reconciliation import");
    assert!(!output.status.success());
    assert_ne!(output.status.code(), Some(101), "CLI must not panic");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("USD total exceeds"),
        "CLI should report a bounded validation error"
    );
}
