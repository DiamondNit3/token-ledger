use std::fs;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use token_ledger::pricing::{
    CatalogCandidateRelation, ManifestEvidenceMetadata, OfficialCatalogManifest, PriceCatalog,
    PricingEngine, PricingError,
};

fn newer_catalog_bytes(revision: &str) -> Vec<u8> {
    catalog_bytes_at(revision, "2026-07-11T00:00:00Z")
}

fn catalog_bytes_at(revision: &str, timestamp: &str) -> Vec<u8> {
    let engine = PricingEngine::bundled().unwrap();
    let mut document: serde_json::Value =
        serde_json::from_slice(engine.catalog().raw_bytes()).unwrap();
    document["revision"] = serde_json::Value::String(revision.to_string());
    document["published_at"] = serde_json::Value::String(timestamp.to_string());
    document["verified_at"] = serde_json::Value::String(timestamp.to_string());
    serde_json::to_vec_pretty(&document).unwrap()
}

fn manifest_for(bytes: &[u8], reference: &str) -> OfficialCatalogManifest {
    let catalog = PriceCatalog::parse(bytes).unwrap();
    OfficialCatalogManifest {
        schema_version: 1,
        catalog_revision: catalog.revision().to_string(),
        catalog_reference: reference.to_string(),
        catalog_sha256: hex::encode(Sha256::digest(bytes)),
        published_at: catalog.published_at(),
        verified_at: catalog.verified_at(),
        evidence: ManifestEvidenceMetadata {
            source_ids: vec![catalog.sources()[0].id.clone()],
            note: "Reviewed official provider evidence for this catalog revision.".to_string(),
        },
    }
}

#[test]
fn bundled_example_manifest_binds_the_bundled_catalog() {
    let manifest_bytes = include_bytes!("../assets/prices.manifest.example.json");
    let digest = hex::encode(Sha256::digest(manifest_bytes));
    let manifest = OfficialCatalogManifest::parse_pinned(manifest_bytes, &digest).unwrap();
    let bundled = PricingEngine::bundled().unwrap();
    let catalog = manifest
        .verify_catalog(bundled.catalog().raw_bytes())
        .unwrap();
    assert_eq!(catalog.sha256(), bundled.catalog().sha256());
}

#[test]
fn update_atomically_replaces_active_and_retains_two_revisions() {
    let directory = tempfile::tempdir().unwrap();
    let active_path = directory.path().join("prices.json");
    let engine = PricingEngine::bundled().unwrap();
    let old_bytes = engine.catalog().raw_bytes().to_vec();
    fs::write(&active_path, &old_bytes).unwrap();
    let new_bytes = newer_catalog_bytes("2026-07-11.integration");
    let checksum = hex::encode(Sha256::digest(&new_bytes));

    let receipt = engine
        .install_candidate(&new_bytes, Some(&checksum), &active_path)
        .unwrap();

    assert_eq!(fs::read(&active_path).unwrap(), new_bytes);
    assert_eq!(receipt.retained_revisions.len(), 2);
    let retained = PricingEngine::retained_revisions(&active_path).unwrap();
    assert_eq!(retained.len(), 2);
    assert!(
        retained
            .iter()
            .any(|item| item.revision == engine.catalog().revision())
    );
    assert!(
        retained
            .iter()
            .any(|item| item.revision == "2026-07-11.integration")
    );
}

#[test]
fn invalid_or_failed_update_never_changes_active_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let active_path = directory.path().join("prices.json");
    let engine = PricingEngine::bundled().unwrap();
    let old_bytes = engine.catalog().raw_bytes().to_vec();
    fs::write(&active_path, &old_bytes).unwrap();

    let mut invalid: serde_json::Value =
        serde_json::from_slice(&newer_catalog_bytes("2026-07-11.invalid")).unwrap();
    invalid["schema_version"] = serde_json::Value::from(999);
    let invalid = serde_json::to_vec_pretty(&invalid).unwrap();
    let parsed = PriceCatalog::parse(&invalid).unwrap();
    assert!(matches!(
        PricingEngine::save_candidate(&parsed, &active_path),
        Err(PricingError::Verification(_))
    ));
    assert_eq!(fs::read(&active_path).unwrap(), old_bytes);

    fs::write(PricingEngine::history_dir(&active_path), b"blocked").unwrap();
    let valid = newer_catalog_bytes("2026-07-11.blocked");
    assert!(matches!(
        engine.install_candidate(&valid, None, &active_path),
        Err(PricingError::Write { .. })
    ));
    assert_eq!(fs::read(&active_path).unwrap(), old_bytes);
}

#[test]
fn first_install_preserves_the_bundled_revision_for_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let active_path = directory.path().join("prices.json");
    let engine = PricingEngine::bundled().unwrap();
    let new_bytes = newer_catalog_bytes("2026-07-11.first-install");

    let receipt = engine
        .install_candidate(&new_bytes, None, &active_path)
        .unwrap();

    assert_eq!(fs::read(&active_path).unwrap(), new_bytes);
    assert_eq!(receipt.retained_revisions.len(), 2);
    assert!(
        receipt
            .retained_revisions
            .iter()
            .any(|item| item.revision == engine.catalog().revision())
    );
}

#[test]
fn check_classifies_current_newer_downgrade_and_revision_conflict_without_mutation() {
    let engine = PricingEngine::bundled().unwrap();
    let active = engine.catalog().raw_bytes().to_vec();

    let current = engine.check_candidate(&active, None).unwrap();
    assert_eq!(current.relation, CatalogCandidateRelation::Current);
    assert!(!current.update_allowed);

    let newer = newer_catalog_bytes("2026-07-11.check");
    let result = engine.check_candidate(&newer, None).unwrap();
    assert_eq!(result.relation, CatalogCandidateRelation::Newer);
    assert!(result.update_allowed);

    let older = catalog_bytes_at("2026-07-09.check", "2026-07-09T00:00:00Z");
    let result = engine.check_candidate(&older, None).unwrap();
    assert_eq!(result.relation, CatalogCandidateRelation::Downgrade);
    assert!(!result.update_allowed);

    let mut conflict: serde_json::Value = serde_json::from_slice(&active).unwrap();
    conflict["coverage_note"] = serde_json::Value::String("different bytes".to_string());
    let conflict = serde_json::to_vec_pretty(&conflict).unwrap();
    let result = engine.check_candidate(&conflict, None).unwrap();
    assert_eq!(result.relation, CatalogCandidateRelation::RevisionConflict);
    assert!(!result.update_allowed);
    assert_eq!(engine.catalog().raw_bytes(), active);
}

#[test]
fn pinned_manifest_rejects_manifest_and_candidate_tampering() {
    let candidate = newer_catalog_bytes("2026-07-11.manifest");
    let manifest = manifest_for(&candidate, "prices.json");
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    let manifest_sha = hex::encode(Sha256::digest(&manifest_bytes));

    let parsed = OfficialCatalogManifest::parse_pinned(&manifest_bytes, &manifest_sha).unwrap();
    assert_eq!(
        parsed.verify_catalog(&candidate).unwrap().revision(),
        "2026-07-11.manifest"
    );

    let mut tampered_manifest = manifest_bytes.clone();
    tampered_manifest.push(b' ');
    assert!(matches!(
        OfficialCatalogManifest::parse_pinned(&tampered_manifest, &manifest_sha),
        Err(PricingError::ChecksumMismatch { .. })
    ));

    let mut tampered_candidate = candidate.clone();
    tampered_candidate.push(b' ');
    assert!(matches!(
        parsed.verify_catalog(&tampered_candidate),
        Err(PricingError::ChecksumMismatch { .. })
    ));
}

#[test]
fn manifest_binds_revision_timestamps_and_catalog_evidence() {
    let candidate = newer_catalog_bytes("2026-07-11.binding");
    let mut manifest = manifest_for(&candidate, "prices.json");
    manifest.catalog_revision = "different-revision".to_string();
    assert!(matches!(
        manifest.verify_catalog(&candidate),
        Err(PricingError::Verification(_))
    ));

    let mut manifest = manifest_for(&candidate, "prices.json");
    manifest.verified_at = "2026-07-12T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
    assert!(matches!(
        manifest.verify_catalog(&candidate),
        Err(PricingError::Verification(_))
    ));

    let mut manifest = manifest_for(&candidate, "prices.json");
    manifest.evidence.source_ids = vec!["not-in-catalog".to_string()];
    assert!(matches!(
        manifest.verify_catalog(&candidate),
        Err(PricingError::Verification(_))
    ));
}

#[test]
fn diff_reports_changed_and_added_records() {
    let directory = tempfile::tempdir().unwrap();
    let active_path = directory.path().join("prices.json");
    let engine = PricingEngine::bundled().unwrap();
    fs::write(&active_path, engine.catalog().raw_bytes()).unwrap();

    let mut candidate: serde_json::Value =
        serde_json::from_slice(&newer_catalog_bytes("2026-07-11.diff")).unwrap();
    candidate["rates"][0]["note"] = serde_json::Value::String("changed rate note".to_string());
    let mut added = candidate["sources"][0].clone();
    added["id"] = serde_json::Value::String("added-source".to_string());
    candidate["sources"].as_array_mut().unwrap().push(added);
    let candidate = serde_json::to_vec_pretty(&candidate).unwrap();
    engine
        .install_candidate(&candidate, None, &active_path)
        .unwrap();

    let diff =
        PricingEngine::diff_revisions(&active_path, engine.catalog().revision(), "2026-07-11.diff")
            .unwrap();
    assert_eq!(diff.sources.added, vec!["added-source"]);
    assert_eq!(diff.rates.changed.len(), 1);
    assert!(diff.metadata_changed.contains(&"published_at".to_string()));
}

#[test]
fn historical_selection_is_read_only_and_activation_is_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let active_path = directory.path().join("prices.json");
    let bundled = PricingEngine::bundled().unwrap();
    fs::write(&active_path, bundled.catalog().raw_bytes()).unwrap();
    let newer = newer_catalog_bytes("2026-07-11.activation");
    bundled
        .install_candidate(&newer, None, &active_path)
        .unwrap();
    let active_before_selection = fs::read(&active_path).unwrap();

    let selected =
        PricingEngine::load_revision(&active_path, bundled.catalog().revision()).unwrap();
    assert_eq!(selected.catalog().sha256(), bundled.catalog().sha256());
    assert_eq!(fs::read(&active_path).unwrap(), active_before_selection);

    let active_engine = PricingEngine::load(&active_path).unwrap();
    let receipt = active_engine
        .activate_revision(bundled.catalog().revision(), &active_path)
        .unwrap();
    assert_eq!(receipt.installed_revision, bundled.catalog().revision());
    assert_eq!(
        fs::read(&active_path).unwrap(),
        bundled.catalog().raw_bytes()
    );
    assert!(
        PricingEngine::retained_revisions(&active_path)
            .unwrap()
            .iter()
            .any(|revision| revision.revision == "2026-07-11.activation")
    );
}

#[test]
fn rollback_uses_published_time_then_revision_and_retains_replaced_active() {
    let directory = tempfile::tempdir().unwrap();
    let active_path = directory.path().join("prices.json");
    let bundled = PricingEngine::bundled().unwrap();
    fs::write(&active_path, bundled.catalog().raw_bytes()).unwrap();

    let middle = catalog_bytes_at("2026-07-11.middle", "2026-07-11T00:00:00Z");
    bundled
        .install_candidate(&middle, None, &active_path)
        .unwrap();
    let middle_engine = PricingEngine::load(&active_path).unwrap();
    let newest = catalog_bytes_at("2026-07-12.newest", "2026-07-12T00:00:00Z");
    middle_engine
        .install_candidate(&newest, None, &active_path)
        .unwrap();

    let newest_engine = PricingEngine::load(&active_path).unwrap();
    let receipt = newest_engine.rollback(&active_path).unwrap();
    assert_eq!(receipt.installed_revision, "2026-07-11.middle");
    assert_eq!(
        PricingEngine::load(&active_path)
            .unwrap()
            .catalog()
            .revision(),
        "2026-07-11.middle"
    );
    assert!(
        PricingEngine::retained_revisions(&active_path)
            .unwrap()
            .iter()
            .any(|revision| revision.revision == "2026-07-12.newest")
    );
}

#[test]
fn failed_historical_activation_preserves_active_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let active_path = directory.path().join("prices.json");
    let bundled = PricingEngine::bundled().unwrap();
    fs::write(&active_path, bundled.catalog().raw_bytes()).unwrap();
    let newer = newer_catalog_bytes("2026-07-11.failure");
    bundled
        .install_candidate(&newer, None, &active_path)
        .unwrap();
    let active_engine = PricingEngine::load(&active_path).unwrap();
    let active_bytes = fs::read(&active_path).unwrap();

    let history = PricingEngine::retained_revisions(&active_path).unwrap();
    let bundled_snapshot = history
        .iter()
        .find(|revision| revision.revision == bundled.catalog().revision())
        .unwrap();
    fs::write(&bundled_snapshot.path, b"tampered").unwrap();
    assert!(
        active_engine
            .activate_revision(bundled.catalog().revision(), &active_path)
            .is_err()
    );
    assert_eq!(fs::read(&active_path).unwrap(), active_bytes);
}
