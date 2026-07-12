use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use rusqlite::Connection;
use tempfile::TempDir;

struct MigrationFixture {
    _temp: TempDir,
    config: PathBuf,
    configured_database: PathBuf,
    target_database: PathBuf,
}

impl MigrationFixture {
    fn create() -> Self {
        let temp = tempfile::tempdir().expect("create migration fixture");
        Self {
            config: temp.path().join("config.toml"),
            configured_database: temp.path().join("configured.sqlite"),
            target_database: temp.path().join("target.sqlite"),
            _temp: temp,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("token-ledger").expect("compiled token-ledger");
        command.arg("--config").arg(&self.config);
        command
    }

    fn initialize(&self) {
        let output = self
            .command()
            .arg("--db")
            .arg(&self.configured_database)
            .args(["init", "--tz", "America/New_York"])
            .output()
            .expect("initialize configured database");
        assert_success(&output);
    }
}

#[test]
fn migrate_requires_explicit_consent_without_touching_v6_rows() {
    let fixture = MigrationFixture::create();
    fixture.initialize();
    make_pre_barrier_v6(&fixture.configured_database);

    let output = fixture
        .command()
        .arg("migrate")
        .output()
        .expect("run migration without consent");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("migration not authorized"));
    assert_eq!(source_count(&fixture.configured_database), 1);
    assert_eq!(schema_version(&fixture.configured_database), 6);
}

#[test]
fn migrate_honors_db_override_authorizes_once_and_is_idempotent() {
    let fixture = MigrationFixture::create();
    fixture.initialize();
    initialize_database(&fixture, &fixture.target_database);
    make_pre_barrier_v6(&fixture.target_database);

    for _ in 0..2 {
        let output = fixture
            .command()
            .arg("--db")
            .arg(&fixture.target_database)
            .args(["migrate", "--accept-history-loss"])
            .output()
            .expect("run authorized migration");
        assert_success(&output);
        assert!(stdout(&output).contains("MIGRATION COMPLETE"));
    }

    assert_eq!(schema_version(&fixture.target_database), 7);
    assert_eq!(source_count(&fixture.target_database), 0);
    assert_eq!(schema_version(&fixture.configured_database), 7);
}

#[test]
fn migrate_accepts_a_fresh_database() {
    let fixture = MigrationFixture::create();
    let output = fixture
        .command()
        .arg("--db")
        .arg(&fixture.target_database)
        .args(["migrate", "--accept-history-loss"])
        .output()
        .expect("migrate fresh database");
    assert_success(&output);
    assert_eq!(schema_version(&fixture.target_database), 7);
}

#[test]
fn migrate_reports_cleanup_failure_while_a_wal_reader_is_live() {
    let fixture = MigrationFixture::create();
    fixture.initialize();
    make_pre_barrier_v6(&fixture.configured_database);

    let blocker = Connection::open(&fixture.configured_database).expect("open WAL blocker");
    blocker
        .execute_batch("BEGIN; SELECT COUNT(*) FROM source_files;")
        .expect("hold read snapshot");

    let output = fixture
        .command()
        .args(["migrate", "--accept-history-loss"])
        .output()
        .expect("run blocked cleanup");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("privacy cleanup could not obtain"));
    drop(blocker);
}

#[test]
fn important_confirmation_scan_and_export_flags_have_help_text() {
    let cases = [
        (
            vec!["init", "--help"],
            vec!["--force", "Rewrite an existing configuration"],
        ),
        (
            vec!["purge", "--help"],
            vec!["--yes", "Confirm permanent removal"],
        ),
        (
            vec!["today", "--help"],
            vec!["--no-scan", "without refreshing local session"],
        ),
        (
            vec!["export", "--help"],
            vec![
                "--group-by",
                "Comma-separated grouping dimensions",
                "--format",
                "Export format",
                "--output",
                "Destination file",
                "--no-scan",
            ],
        ),
    ];

    for (args, expected) in cases {
        let output = Command::cargo_bin("token-ledger")
            .expect("compiled token-ledger")
            .args(&args)
            .output()
            .unwrap_or_else(|error| panic!("run {args:?}: {error}"));
        assert_success(&output);
        let help = stdout(&output);
        for text in expected {
            assert!(
                help.contains(text),
                "{args:?} help omitted {text:?}:\n{help}"
            );
        }
    }
}

fn initialize_database(fixture: &MigrationFixture, database: &Path) {
    let temporary_config = fixture._temp.path().join("target-config.toml");
    let output = Command::cargo_bin("token-ledger")
        .expect("compiled token-ledger")
        .arg("--config")
        .arg(temporary_config)
        .arg("--db")
        .arg(database)
        .args(["init", "--tz", "America/New_York"])
        .output()
        .expect("initialize target database");
    assert_success(&output);
}

fn make_pre_barrier_v6(database: &Path) {
    let connection = Connection::open(database).expect("open initialized ledger");
    connection
        .execute(
            "INSERT INTO source_files(client, path, privacy_write_generation) VALUES ('claude_code', 'tlsrc_migration_fixture', 1)",
            [],
        )
        .expect("insert cached source row");
    connection
        .execute_batch(
            "DROP TRIGGER IF EXISTS guard_schema_version_no_downgrade;
             DROP TRIGGER IF EXISTS guard_schema_version_no_delete;
             DROP TRIGGER IF EXISTS guard_schema_version_no_replace;
             UPDATE meta SET value='6' WHERE key='schema_version';
             DELETE FROM meta WHERE key='v7_privacy_barrier';",
        )
        .expect("emulate pre-barrier v6 database");
}

fn schema_version(database: &Path) -> i64 {
    Connection::open(database)
        .expect("open ledger")
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("read schema version")
}

fn source_count(database: &Path) -> i64 {
    Connection::open(database)
        .expect("open ledger")
        .query_row("SELECT COUNT(*) FROM source_files", [], |row| row.get(0))
        .expect("read source count")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}
