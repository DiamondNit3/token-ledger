use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::billing::BillingEvidence;

/// A user-attested pricing dimension that applies only during a bounded
/// interval. The interval is half-open: `effective_from <= event < effective_to`.
///
/// These records are deliberately explicit and auditable. They are never
/// inferred from a provider default and they never alter the source event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingDimensionOverride {
    /// Stable user-chosen identifier included in pricing provenance.
    pub id: String,
    pub provider: String,
    pub canonical_model: String,
    pub dimension: String,
    pub value: String,
    pub effective_from: DateTime<Utc>,
    pub effective_to: DateTime<Utc>,
    pub attested_at: DateTime<Utc>,
    #[serde(default)]
    pub note: Option<String>,
}

impl PricingDimensionOverride {
    pub fn contains(&self, occurred_at: DateTime<Utc>) -> bool {
        occurred_at >= self.effective_from && occurred_at < self.effective_to
    }

    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("id", self.id.as_str()),
            ("provider", self.provider.as_str()),
            ("canonical_model", self.canonical_model.as_str()),
            ("dimension", self.dimension.as_str()),
            ("value", self.value.as_str()),
        ] {
            anyhow::ensure!(!value.trim().is_empty(), "override {name} cannot be empty");
        }
        anyhow::ensure!(
            self.effective_from < self.effective_to,
            "pricing override '{}' must have effective_from before effective_to",
            self.id
        );
        anyhow::ensure!(
            matches!(
                self.dimension.trim().to_ascii_lowercase().as_str(),
                "auth_mode" | "provider_route" | "service_tier" | "speed" | "inference_geo"
            ),
            "pricing override '{}' uses unsupported dimension '{}'",
            self.id,
            self.dimension
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathOrigin {
    Cli,
    Environment,
    Config,
    Default,
}

impl std::fmt::Display for PathOrigin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Cli => "CLI override",
            Self::Environment => "environment",
            Self::Config => "config file",
            Self::Default => "platform default",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub path: PathBuf,
    pub origin: PathOrigin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub timezone: String,
    pub database_path: Option<PathBuf>,
    pub claude_root: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    pub price_catalog: Option<PathBuf>,
    /// Optional immutable catalog revision used for pricing-backed commands.
    /// This selects a verified retained revision without changing the active
    /// catalog file.
    pub catalog_revision: Option<String>,
    /// Explicitly configured official catalog-manifest location. Network
    /// access occurs only for `prices check --official` or
    /// `prices update --official`.
    pub official_price_manifest: Option<String>,
    /// Trusted SHA-256 of the exact official manifest bytes. This pin is
    /// required even when the manifest is a local file.
    pub official_price_manifest_sha256: Option<String>,
    /// Optional, bounded user attestations used by scenario-aware pricing.
    #[serde(default)]
    pub pricing_dimension_overrides: Vec<PricingDimensionOverride>,
    /// Explicit cash records and completeness attestations. These are kept
    /// separate from API-equivalent pricing estimates.
    #[serde(default)]
    pub billing_evidence: BillingEvidence,
    /// Show full stored pseudonyms instead of shortened display references.
    /// Provider-native identifiers are never retained in the ledger.
    pub show_raw_ids: bool,
    /// Transient command-line overrides; deliberately never persisted.
    #[serde(skip)]
    pub claude_root_override: Option<PathBuf>,
    #[serde(skip)]
    pub codex_home_override: Option<PathBuf>,
    /// Transient CLI catalog revision selector; deliberately never persisted.
    #[serde(skip)]
    pub catalog_revision_override: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timezone: iana_time_zone().unwrap_or_else(|| "UTC".to_string()),
            database_path: None,
            claude_root: None,
            codex_home: None,
            price_catalog: None,
            catalog_revision: None,
            official_price_manifest: None,
            official_price_manifest_sha256: None,
            pricing_dimension_overrides: Vec::new(),
            billing_evidence: BillingEvidence::default(),
            show_raw_ids: false,
            claude_root_override: None,
            codex_home_override: None,
            catalog_revision_override: None,
        }
    }
}

impl Config {
    pub fn project_dirs() -> Result<ProjectDirs> {
        ProjectDirs::from("dev", "token-ledger", "Token Ledger")
            .context("could not determine platform application directories")
    }

    pub fn default_config_path() -> Result<PathBuf> {
        Ok(Self::project_dirs()?.config_dir().join("config.toml"))
    }

    pub fn default_database_path() -> Result<PathBuf> {
        Ok(Self::project_dirs()?
            .data_local_dir()
            .join("ledger.sqlite3"))
    }

    pub fn load(path: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or(Self::default_config_path()?);
        if !path
            .try_exists()
            .with_context(|| format!("failed to inspect config {}", path.display()))?
        {
            return Ok((Self::default(), path));
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        config.validate_pricing_dimension_overrides()?;
        config.validate_billing_evidence()?;
        Ok((config, path))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate_pricing_dimension_overrides()?;
        self.validate_billing_evidence()?;
        if let Some(parent) = path.parent().filter(|value| !value.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self)?;
        fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn resolved_database_path(&self) -> Result<PathBuf> {
        self.database_path
            .clone()
            .map(Ok)
            .unwrap_or_else(Self::default_database_path)
    }

    pub fn validate_pricing_dimension_overrides(&self) -> Result<()> {
        let mut ids = std::collections::BTreeSet::new();
        for override_value in &self.pricing_dimension_overrides {
            override_value.validate()?;
            anyhow::ensure!(
                ids.insert(override_value.id.clone()),
                "pricing override id '{}' is duplicated",
                override_value.id
            );
        }

        for (index, left) in self.pricing_dimension_overrides.iter().enumerate() {
            for right in self.pricing_dimension_overrides.iter().skip(index + 1) {
                let same_target = left.provider.eq_ignore_ascii_case(&right.provider)
                    && left.canonical_model == right.canonical_model
                    && left.dimension.eq_ignore_ascii_case(&right.dimension);
                let overlaps = left.effective_from < right.effective_to
                    && right.effective_from < left.effective_to;
                anyhow::ensure!(
                    !(same_target && overlaps),
                    "pricing overrides '{}' and '{}' overlap for the same target",
                    left.id,
                    right.id
                );
            }
        }
        Ok(())
    }

    pub fn validate_billing_evidence(&self) -> Result<()> {
        self.billing_evidence
            .validate()
            .context("invalid billing evidence configuration")
    }
}

fn iana_time_zone() -> Option<String> {
    std::env::var("TZ")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| iana_time_zone::get_timezone().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn bounded_override(id: &str) -> PricingDimensionOverride {
        PricingDimensionOverride {
            id: id.to_string(),
            provider: "anthropic".to_string(),
            canonical_model: "claude-fable-5".to_string(),
            dimension: "inference_geo".to_string(),
            value: "global".to_string(),
            effective_from: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
            effective_to: Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
            attested_at: Utc.with_ymd_and_hms(2026, 7, 10, 0, 0, 0).unwrap(),
            note: Some("user checked organization routing settings".to_string()),
        }
    }

    #[test]
    fn override_interval_is_half_open() {
        let value = bounded_override("geo-july");
        assert!(value.contains(value.effective_from));
        assert!(!value.contains(value.effective_to));
    }

    #[test]
    fn config_round_trips_attestation_provenance() {
        let config = Config {
            pricing_dimension_overrides: vec![bounded_override("geo-july")],
            ..Config::default()
        };
        let text = toml::to_string(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed.pricing_dimension_overrides,
            config.pricing_dimension_overrides
        );
    }

    #[test]
    fn rejects_unbounded_or_overlapping_attestations() {
        let mut invalid = bounded_override("invalid");
        invalid.effective_to = invalid.effective_from;
        assert!(invalid.validate().is_err());

        let config = Config {
            pricing_dimension_overrides: vec![
                bounded_override("first"),
                bounded_override("second"),
            ],
            ..Config::default()
        };
        assert!(config.validate_pricing_dimension_overrides().is_err());
    }
}
