use std::fs;

use assert_cmd::Command;
use tempfile::TempDir;

fn demo_command() -> Command {
    let mut command = Command::cargo_bin("token-ledger").expect("compiled token-ledger");
    command
        .env("TOKEN_LEDGER_WIDTH", "120")
        .args(["--color", "never", "--unicode", "never"]);
    command
}

#[test]
fn demo_output_is_stable_useful_and_explicitly_synthetic() {
    let first = demo_command()
        .arg("demo")
        .output()
        .expect("first demo invocation");
    let second = demo_command()
        .arg("demo")
        .output()
        .expect("second demo invocation");

    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
    assert!(first.stderr.is_empty());

    let stdout = String::from_utf8(first.stdout).expect("demo output is UTF-8");
    for expected in [
        "TOKEN LEDGER / DEMO",
        "Synthetic data only",
        "API-equivalent list-price value",
        "Claude Fable 5",
        "GPT-5.6 Sol",
        "PROVIDER UNITS",
        "BILLING EVIDENCE",
        "[INCOMPLETE EVIDENCE]",
        "Snapshot 2026-07-11 16:00 UTC",
        "TRY IT WITH YOUR DATA",
    ] {
        assert!(
            stdout.contains(expected),
            "missing `{expected}` in:\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("Not priced"),
        "demo should exercise priced models"
    );
    assert!(!stdout.contains("\u{1b}["), "no ANSI was requested");
}

#[test]
fn demo_bypasses_poisoned_config_database_and_session_roots() {
    let temp = TempDir::new().expect("demo isolation directory");
    let config = temp.path().join("poisoned.toml");
    let database = temp.path().join("must-not-exist.sqlite3");
    let claude_root = temp.path().join("claude-private-canary");
    let codex_home = temp.path().join("codex-private-canary");
    fs::write(&config, "this is deliberately not valid = [toml").expect("write poisoned config");
    fs::create_dir_all(&claude_root).expect("create Claude canary root");
    fs::create_dir_all(&codex_home).expect("create Codex canary root");
    fs::write(claude_root.join("PRIVATE_CLAUDE_CANARY.jsonl"), "not json")
        .expect("write Claude canary");
    fs::write(codex_home.join("PRIVATE_CODEX_CANARY.jsonl"), "not json")
        .expect("write Codex canary");

    let output = demo_command()
        .arg("--config")
        .arg(&config)
        .arg("--db")
        .arg(&database)
        .arg("--claude-root")
        .arg(&claude_root)
        .arg("--codex-home")
        .arg(&codex_home)
        .arg("demo")
        .output()
        .expect("isolated demo invocation");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!database.exists(), "demo created the database override");
    let stdout = String::from_utf8(output.stdout).expect("demo output is UTF-8");
    assert!(!stdout.contains("PRIVATE_CLAUDE_CANARY"));
    assert!(!stdout.contains("PRIVATE_CODEX_CANARY"));
    assert!(stdout.contains("no config, database, or session roots were read"));
}

#[test]
fn demo_is_discoverable_from_help() {
    let output = demo_command()
        .arg("--help")
        .output()
        .expect("CLI help invocation");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");
    assert!(stdout.contains("demo"));
    assert!(stdout.contains("deterministic synthetic data"));
}
