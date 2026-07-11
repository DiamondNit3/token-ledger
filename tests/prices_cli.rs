use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use token_ledger::config::Config;
use token_ledger::pricing::{
    ManifestEvidenceMetadata, OfficialCatalogManifest, PriceCatalog, PricingEngine,
};

struct PriceFixture {
    _temp: TempDir,
    config_path: PathBuf,
    active_path: PathBuf,
    database_path: PathBuf,
}

impl PriceFixture {
    fn create() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        let active_path = temp.path().join("prices.json");
        let database_path = temp.path().join("ledger.sqlite3");
        fs::write(
            &active_path,
            PricingEngine::bundled().unwrap().catalog().raw_bytes(),
        )
        .unwrap();
        let config = Config {
            timezone: "UTC".to_string(),
            price_catalog: Some(active_path.clone()),
            database_path: Some(database_path.clone()),
            ..Config::default()
        };
        config.save(&config_path).unwrap();
        Self {
            _temp: temp,
            config_path,
            active_path,
            database_path,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("ledger").unwrap();
        command.arg("--config").arg(&self.config_path);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "ledger {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn configure_manifest(&self, manifest_path: &Path, digest: &str) {
        let (mut config, _) = Config::load(Some(&self.config_path)).unwrap();
        config.official_price_manifest = Some(manifest_path.display().to_string());
        config.official_price_manifest_sha256 = Some(digest.to_string());
        config.save(&self.config_path).unwrap();
    }
}

fn catalog_bytes(revision: &str, timestamp: &str) -> Vec<u8> {
    let bundled = PricingEngine::bundled().unwrap();
    let mut document: Value = serde_json::from_slice(bundled.catalog().raw_bytes()).unwrap();
    document["revision"] = Value::String(revision.to_string());
    document["published_at"] = Value::String(timestamp.to_string());
    document["verified_at"] = Value::String(timestamp.to_string());
    serde_json::to_vec_pretty(&document).unwrap()
}

fn manifest_bytes(candidate: &[u8], reference: &str) -> Vec<u8> {
    let catalog = PriceCatalog::parse(candidate).unwrap();
    serde_json::to_vec_pretty(&OfficialCatalogManifest {
        schema_version: 1,
        catalog_revision: catalog.revision().to_string(),
        catalog_reference: reference.to_string(),
        catalog_sha256: hex::encode(Sha256::digest(candidate)),
        published_at: catalog.published_at(),
        verified_at: catalog.verified_at(),
        evidence: ManifestEvidenceMetadata {
            source_ids: vec![catalog.sources()[0].id.clone()],
            note: "Reviewed official pricing evidence.".to_string(),
        },
    })
    .unwrap()
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON output: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn lifecycle_cli_checks_updates_diffs_selects_rolls_back_and_activates() {
    let fixture = PriceFixture::create();
    let bundled_revision = PricingEngine::bundled()
        .unwrap()
        .catalog()
        .revision()
        .to_string();
    let candidate = catalog_bytes("2026-07-11.cli", "2026-07-11T00:00:00Z");
    let candidate_path = fixture._temp.path().join("candidate.json");
    fs::write(&candidate_path, &candidate).unwrap();
    let candidate_path_text = candidate_path.display().to_string();
    let sha = hex::encode(Sha256::digest(&candidate));
    let active_before = fs::read(&fixture.active_path).unwrap();

    let check = fixture.run(&[
        "prices",
        "check",
        "--from",
        &candidate_path_text,
        "--sha256",
        &sha,
        "--json",
    ]);
    assert_eq!(stdout_json(&check)["check"]["relation"], "newer");
    assert_eq!(fs::read(&fixture.active_path).unwrap(), active_before);

    let update = fixture.run(&[
        "prices",
        "update",
        "--from",
        &candidate_path_text,
        "--sha256",
        &sha,
        "--json",
    ]);
    assert_eq!(stdout_json(&update)["installed_revision"], "2026-07-11.cli");

    let selected = fixture.run(&[
        "--catalog-revision",
        &bundled_revision,
        "prices",
        "status",
        "--json",
    ]);
    assert_eq!(stdout_json(&selected)["revision"], bundled_revision);
    assert_eq!(fs::read(&fixture.active_path).unwrap(), candidate);

    let day = fixture.run(&[
        "--catalog-revision",
        &bundled_revision,
        "day",
        "2026-07-10",
        "--no-scan",
        "--json",
    ]);
    assert_eq!(stdout_json(&day)["catalog"]["revision"], bundled_revision);
    assert!(fixture.database_path.exists());

    let (mut configured, _) = Config::load(Some(&fixture.config_path)).unwrap();
    configured.catalog_revision = Some(bundled_revision.clone());
    configured.save(&fixture.config_path).unwrap();
    let configured_day = fixture.run(&["day", "2026-07-10", "--no-scan", "--json"]);
    assert_eq!(
        stdout_json(&configured_day)["catalog"]["revision"],
        bundled_revision
    );
    configured.catalog_revision = None;
    configured.save(&fixture.config_path).unwrap();

    let diff = fixture.run(&[
        "prices",
        "diff",
        &bundled_revision,
        "2026-07-11.cli",
        "--json",
    ]);
    assert_eq!(stdout_json(&diff)["to_revision"], "2026-07-11.cli");

    let rollback = fixture.run(&["prices", "rollback"]);
    let rollback_text = String::from_utf8_lossy(&rollback.stdout);
    assert!(rollback_text.contains(&format!("Activated {bundled_revision}")));
    assert!(rollback_text.contains("retained replaced active revision 2026-07-11.cli"));

    fixture.run(&[
        "prices",
        "update",
        "--from",
        &candidate_path_text,
        "--sha256",
        &sha,
    ]);
    let activate = fixture.run(&["prices", "activate", &bundled_revision]);
    assert!(
        String::from_utf8_lossy(&activate.stdout).contains("Historical activation was explicit")
    );
    assert_eq!(
        PricingEngine::load(&fixture.active_path)
            .unwrap()
            .catalog()
            .revision(),
        bundled_revision
    );
}

#[test]
fn official_cli_requires_and_verifies_pinned_manifest_and_candidate() {
    let fixture = PriceFixture::create();
    let candidate = catalog_bytes("2026-07-12.official", "2026-07-12T00:00:00Z");
    let candidate_path = fixture._temp.path().join("official-prices.json");
    fs::write(&candidate_path, &candidate).unwrap();
    let manifest = manifest_bytes(&candidate, "official-prices.json");
    let manifest_path = fixture._temp.path().join("official-manifest.json");
    fs::write(&manifest_path, &manifest).unwrap();
    let manifest_sha = hex::encode(Sha256::digest(&manifest));
    fixture.configure_manifest(&manifest_path, &manifest_sha);

    let check = fixture.run(&["prices", "check", "--official", "--json"]);
    let check = stdout_json(&check);
    assert_eq!(check["check"]["relation"], "newer");
    assert_eq!(check["manifest"]["catalog_revision"], "2026-07-12.official");
    assert!(
        check["trust"]
            .as_str()
            .unwrap()
            .contains("not a cryptographic signature")
    );

    let update = fixture.run(&["prices", "update", "--official", "--json"]);
    assert_eq!(
        stdout_json(&update)["installed_revision"],
        "2026-07-12.official"
    );
}

#[test]
fn official_cli_tamper_failure_preserves_active_catalog() {
    let fixture = PriceFixture::create();
    let candidate = catalog_bytes("2026-07-12.tamper", "2026-07-12T00:00:00Z");
    let candidate_path = fixture._temp.path().join("candidate.json");
    fs::write(&candidate_path, &candidate).unwrap();
    let manifest = manifest_bytes(&candidate, "candidate.json");
    let manifest_path = fixture._temp.path().join("manifest.json");
    fs::write(&manifest_path, &manifest).unwrap();
    fixture.configure_manifest(&manifest_path, &hex::encode(Sha256::digest(&manifest)));
    let active_before = fs::read(&fixture.active_path).unwrap();
    fs::write(&candidate_path, b"tampered candidate").unwrap();

    let output = fixture
        .command()
        .args(["prices", "update", "--official"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("checksum mismatch"));
    assert_eq!(fs::read(&fixture.active_path).unwrap(), active_before);
}
