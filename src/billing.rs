//! User-attested cash billing evidence.
//!
//! This module intentionally does not consume token prices. API list-price
//! equivalents and provider credits are usage estimates; the values here are
//! explicit records of money paid (or refunded). `recorded_cash_usd` is always
//! available, but it becomes `actual_billed_usd` only when bounded completeness
//! attestations cover the entire requested provider scope and time window.
//!
//! Evidence stores no credentials, account identifiers, invoice identifiers,
//! or statement contents. `source_note` is a short, non-secret human label such
//! as "checked provider billing page"; callers must not put secrets in it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_ID_CHARS: usize = 128;
const MAX_PROVIDER_CHARS: usize = 64;
const MAX_SOURCE_NOTE_CHARS: usize = 240;
const MAX_RECURRING_OCCURRENCES: u32 = 10_000;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BillingError {
    #[error("invalid billing evidence: {0}")]
    InvalidEvidence(String),
    #[error("invalid billing query: {0}")]
    InvalidQuery(String),
    #[error("billing decimal arithmetic overflowed")]
    ArithmeticOverflow,
}

/// A half-open UTC interval: `start <= instant < end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillingWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl BillingWindow {
    pub fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Self, BillingError> {
        let value = Self { start, end };
        value.validate()?;
        Ok(value)
    }

    pub fn contains(&self, instant: DateTime<Utc>) -> bool {
        instant >= self.start && instant < self.end
    }

    pub fn validate(&self) -> Result<(), BillingError> {
        if self.start >= self.end {
            return Err(BillingError::InvalidQuery(
                "window start must be before window end".to_string(),
            ));
        }
        Ok(())
    }
}

/// A query over recorded billing evidence.
///
/// An empty provider set means "show every recorded provider", but is an open
/// world and can never produce an attested-complete combined actual cost. Use
/// [`BillingQuery::for_providers`] with every intended provider to close scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillingQuery {
    pub window: BillingWindow,
    #[serde(default)]
    pub providers: BTreeSet<String>,
}

impl BillingQuery {
    pub fn all_recorded(window: BillingWindow) -> Self {
        Self {
            window,
            providers: BTreeSet::new(),
        }
    }

    pub fn for_providers<I, S>(window: BillingWindow, providers: I) -> Result<Self, BillingError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut normalized = BTreeSet::new();
        for provider in providers {
            normalized.insert(normalize_provider_for_query(provider.as_ref())?);
        }
        if normalized.is_empty() {
            return Err(BillingError::InvalidQuery(
                "an attested actual total requires at least one provider".to_string(),
            ));
        }
        let value = Self {
            window,
            providers: normalized,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), BillingError> {
        self.window.validate()?;
        for provider in &self.providers {
            validate_provider(provider).map_err(BillingError::InvalidQuery)?;
        }
        Ok(())
    }

    pub fn has_closed_provider_scope(&self) -> bool {
        !self.providers.is_empty()
    }
}

/// The reason a user-provided cash movement occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingCategory {
    ApiCharge,
    CreditPurchase,
    SubscriptionPlan,
    Tax,
    Refund,
    Other,
}

/// A single observed cash movement. Positive values are money paid; refunds
/// are negative and must use [`BillingCategory::Refund`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OneTimeBillingRecord {
    pub id: String,
    pub provider: String,
    pub category: BillingCategory,
    #[serde(with = "decimal_string")]
    pub amount_usd: Decimal,
    pub charged_at: DateTime<Utc>,
    pub attested_at: DateTime<Utc>,
    pub source_note: String,
}

impl OneTimeBillingRecord {
    pub fn validate(&self) -> Result<(), BillingError> {
        validate_id(&self.id).map_err(BillingError::InvalidEvidence)?;
        validate_provider(&self.provider).map_err(BillingError::InvalidEvidence)?;
        validate_source_note(&self.source_note).map_err(BillingError::InvalidEvidence)?;
        if self.attested_at < self.charged_at {
            return Err(BillingError::InvalidEvidence(format!(
                "one-time record '{}' was attested before it was charged",
                self.id
            )));
        }
        match self.category {
            BillingCategory::Refund if self.amount_usd >= Decimal::ZERO => {
                return Err(BillingError::InvalidEvidence(format!(
                    "refund record '{}' must have a negative amount_usd",
                    self.id
                )));
            }
            BillingCategory::Refund => {}
            _ if self.amount_usd <= Decimal::ZERO => {
                return Err(BillingError::InvalidEvidence(format!(
                    "non-refund record '{}' must have a positive amount_usd",
                    self.id
                )));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingCadence {
    Monthly,
    Annual,
}

impl BillingCadence {
    fn months(self) -> u32 {
        match self {
            Self::Monthly => 1,
            Self::Annual => 12,
        }
    }
}

/// A bounded series of user-attested subscription charges.
///
/// `effective_from` is the first charge instant. Later charge dates preserve
/// that anchor's day where possible and clamp to the target month's final day
/// (January 31 becomes February 28/29, then March 31). `effective_to` is
/// exclusive. The series is historical evidence rather than a forecast, so it
/// must be attested no earlier than `effective_to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurringPlanCharge {
    pub id: String,
    pub provider: String,
    #[serde(with = "decimal_string")]
    pub amount_usd: Decimal,
    pub cadence: BillingCadence,
    pub effective_from: DateTime<Utc>,
    pub effective_to: DateTime<Utc>,
    pub attested_at: DateTime<Utc>,
    pub source_note: String,
}

impl RecurringPlanCharge {
    pub fn validate(&self) -> Result<(), BillingError> {
        validate_id(&self.id).map_err(BillingError::InvalidEvidence)?;
        validate_provider(&self.provider).map_err(BillingError::InvalidEvidence)?;
        validate_source_note(&self.source_note).map_err(BillingError::InvalidEvidence)?;
        if self.amount_usd <= Decimal::ZERO {
            return Err(BillingError::InvalidEvidence(format!(
                "recurring plan '{}' must have a positive amount_usd",
                self.id
            )));
        }
        if self.effective_from >= self.effective_to {
            return Err(BillingError::InvalidEvidence(format!(
                "recurring plan '{}' must have effective_from before effective_to",
                self.id
            )));
        }
        if self.attested_at < self.effective_to {
            return Err(BillingError::InvalidEvidence(format!(
                "recurring plan '{}' must be attested at or after effective_to",
                self.id
            )));
        }
        // Validate that the bounded series is representable and not abusive.
        let full_window = BillingWindow {
            start: self.effective_from,
            end: self.effective_to,
        };
        self.charge_instants_in_unchecked(&full_window)?;
        Ok(())
    }

    /// Returns charge instants inside the intersection of the plan and a
    /// half-open query window. Values are cash-flow instants, never prorations.
    pub fn charge_instants_in(
        &self,
        window: &BillingWindow,
    ) -> Result<Vec<DateTime<Utc>>, BillingError> {
        window.validate()?;
        self.validate()?;
        self.charge_instants_in_unchecked(window)
    }

    fn charge_instants_in_unchecked(
        &self,
        window: &BillingWindow,
    ) -> Result<Vec<DateTime<Utc>>, BillingError> {
        let stop = self.effective_to.min(window.end);
        if stop <= self.effective_from || window.end <= self.effective_from {
            return Ok(Vec::new());
        }

        let mut values = Vec::new();
        for index in 0..MAX_RECURRING_OCCURRENCES {
            let months = index.checked_mul(self.cadence.months()).ok_or_else(|| {
                BillingError::InvalidEvidence(format!(
                    "recurring plan '{}' has too many occurrences",
                    self.id
                ))
            })?;
            let instant = add_months_anchored(self.effective_from, months).ok_or_else(|| {
                BillingError::InvalidEvidence(format!(
                    "recurring plan '{}' exceeds supported calendar range",
                    self.id
                ))
            })?;
            if instant >= stop {
                return Ok(values);
            }
            if instant >= window.start {
                values.push(instant);
            }
        }

        Err(BillingError::InvalidEvidence(format!(
            "recurring plan '{}' exceeds {} occurrences",
            self.id, MAX_RECURRING_OCCURRENCES
        )))
    }
}

/// A user's assertion that all cash billing activity for one provider is
/// represented by the evidence for the bounded interval. It is intentionally
/// provider-wide; category-only attestations cannot prove an actual total.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillingCompletenessAttestation {
    pub id: String,
    pub provider: String,
    pub effective_from: DateTime<Utc>,
    pub effective_to: DateTime<Utc>,
    pub attested_at: DateTime<Utc>,
    pub source_note: String,
}

impl BillingCompletenessAttestation {
    pub fn validate(&self) -> Result<(), BillingError> {
        validate_id(&self.id).map_err(BillingError::InvalidEvidence)?;
        validate_provider(&self.provider).map_err(BillingError::InvalidEvidence)?;
        validate_source_note(&self.source_note).map_err(BillingError::InvalidEvidence)?;
        if self.effective_from >= self.effective_to {
            return Err(BillingError::InvalidEvidence(format!(
                "completeness attestation '{}' must have effective_from before effective_to",
                self.id
            )));
        }
        if self.attested_at < self.effective_to {
            return Err(BillingError::InvalidEvidence(format!(
                "completeness attestation '{}' must be attested at or after effective_to",
                self.id
            )));
        }
        Ok(())
    }
}

/// Config-compatible, entirely user-provided billing evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BillingEvidence {
    pub one_time_charges: Vec<OneTimeBillingRecord>,
    pub recurring_plan_charges: Vec<RecurringPlanCharge>,
    pub completeness_attestations: Vec<BillingCompletenessAttestation>,
}

impl BillingEvidence {
    pub fn validate(&self) -> Result<(), BillingError> {
        let mut ids = BTreeSet::new();
        for record in &self.one_time_charges {
            record.validate()?;
            ensure_unique_id(&mut ids, &record.id)?;
        }
        for plan in &self.recurring_plan_charges {
            plan.validate()?;
            ensure_unique_id(&mut ids, &plan.id)?;
        }
        for attestation in &self.completeness_attestations {
            attestation.validate()?;
            ensure_unique_id(&mut ids, &attestation.id)?;
        }

        for (index, left) in self.completeness_attestations.iter().enumerate() {
            for right in self.completeness_attestations.iter().skip(index + 1) {
                let same_provider = left.provider == right.provider;
                let overlaps = left.effective_from < right.effective_to
                    && right.effective_from < left.effective_to;
                if same_provider && overlaps {
                    return Err(BillingError::InvalidEvidence(format!(
                        "completeness attestations '{}' and '{}' overlap for provider '{}'",
                        left.id, right.id, left.provider
                    )));
                }
            }
        }
        Ok(())
    }

    /// Aggregate recorded cash and, only with complete evidence, actual billed
    /// cash for a half-open window and optional provider filter.
    pub fn aggregate(&self, query: &BillingQuery) -> Result<BillingAggregate, BillingError> {
        self.validate()?;
        query.validate()?;

        let closed_provider_scope = query.has_closed_provider_scope();
        let providers = if closed_provider_scope {
            query.providers.clone()
        } else {
            self.all_provider_ids()
        };

        let mut by_provider = providers
            .iter()
            .map(|provider| {
                (
                    provider.clone(),
                    ProviderBillingTotal::empty(provider.clone()),
                )
            })
            .collect::<BTreeMap<_, _>>();

        for record in &self.one_time_charges {
            if query.window.contains(record.charged_at) && providers.contains(&record.provider) {
                let total = by_provider
                    .get_mut(&record.provider)
                    .expect("selected provider total exists");
                total.add(record.category, record.amount_usd, false)?;
            }
        }

        for plan in &self.recurring_plan_charges {
            if !providers.contains(&plan.provider) {
                continue;
            }
            let count = plan.charge_instants_in_unchecked(&query.window)?.len();
            let total = by_provider
                .get_mut(&plan.provider)
                .expect("selected provider total exists");
            for _ in 0..count {
                total.add(BillingCategory::SubscriptionPlan, plan.amount_usd, true)?;
            }
        }

        let mut recorded_cash_usd = Decimal::ZERO;
        let mut all_selected_providers_complete = closed_provider_scope;
        for total in by_provider.values_mut() {
            total.attested_complete =
                self.provider_window_is_complete(&total.provider, query.window);
            total.actual_billed_usd = total.attested_complete.then_some(total.recorded_cash_usd);
            all_selected_providers_complete &= total.attested_complete;
            checked_add(&mut recorded_cash_usd, total.recorded_cash_usd)?;
        }

        let actual_billing_status = if !closed_provider_scope {
            ActualBillingStatus::OpenProviderScope
        } else if all_selected_providers_complete {
            ActualBillingStatus::AttestedComplete
        } else {
            ActualBillingStatus::IncompleteEvidence
        };
        let actual_billed_usd = all_selected_providers_complete.then_some(recorded_cash_usd);

        Ok(BillingAggregate {
            window: query.window,
            provider_scope: providers,
            recorded_cash_usd,
            actual_billed_usd,
            actual_billing_status,
            by_provider,
        })
    }

    fn all_provider_ids(&self) -> BTreeSet<String> {
        self.one_time_charges
            .iter()
            .map(|value| value.provider.clone())
            .chain(
                self.recurring_plan_charges
                    .iter()
                    .map(|value| value.provider.clone()),
            )
            .chain(
                self.completeness_attestations
                    .iter()
                    .map(|value| value.provider.clone()),
            )
            .collect()
    }

    fn provider_window_is_complete(&self, provider: &str, window: BillingWindow) -> bool {
        let mut intervals = self
            .completeness_attestations
            .iter()
            .filter(|value| value.provider == provider)
            .collect::<Vec<_>>();
        intervals.sort_by_key(|value| value.effective_from);

        let mut cursor = window.start;
        for interval in intervals {
            if interval.effective_to <= cursor {
                continue;
            }
            if interval.effective_from > cursor {
                return false;
            }
            cursor = interval.effective_to;
            if cursor >= window.end {
                return true;
            }
        }
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActualBillingStatus {
    /// Every explicitly selected provider has complete bounded evidence.
    AttestedComplete,
    /// At least one selected provider lacks completeness coverage.
    IncompleteEvidence,
    /// No provider filter was supplied, so omitted providers cannot be ruled out.
    OpenProviderScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBillingTotal {
    pub provider: String,
    #[serde(with = "decimal_string")]
    pub recorded_cash_usd: Decimal,
    #[serde(default, with = "optional_decimal_string")]
    pub actual_billed_usd: Option<Decimal>,
    pub attested_complete: bool,
    pub one_time_charge_count: u64,
    pub recurring_charge_count: u64,
    #[serde(with = "decimal_map")]
    pub by_category: BTreeMap<BillingCategory, Decimal>,
}

impl ProviderBillingTotal {
    fn empty(provider: String) -> Self {
        Self {
            provider,
            recorded_cash_usd: Decimal::ZERO,
            actual_billed_usd: None,
            attested_complete: false,
            one_time_charge_count: 0,
            recurring_charge_count: 0,
            by_category: BTreeMap::new(),
        }
    }

    fn add(
        &mut self,
        category: BillingCategory,
        amount: Decimal,
        recurring: bool,
    ) -> Result<(), BillingError> {
        checked_add(&mut self.recorded_cash_usd, amount)?;
        let category_total = self.by_category.entry(category).or_insert(Decimal::ZERO);
        checked_add(category_total, amount)?;
        if recurring {
            self.recurring_charge_count = self
                .recurring_charge_count
                .checked_add(1)
                .ok_or(BillingError::ArithmeticOverflow)?;
        } else {
            self.one_time_charge_count = self
                .one_time_charge_count
                .checked_add(1)
                .ok_or(BillingError::ArithmeticOverflow)?;
        }
        Ok(())
    }
}

/// Cash evidence only. It deliberately has no API-equivalent or token-pricing
/// field, preventing callers from accidentally presenting an estimate as paid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillingAggregate {
    pub window: BillingWindow,
    pub provider_scope: BTreeSet<String>,
    #[serde(with = "decimal_string")]
    pub recorded_cash_usd: Decimal,
    #[serde(default, with = "optional_decimal_string")]
    pub actual_billed_usd: Option<Decimal>,
    pub actual_billing_status: ActualBillingStatus,
    pub by_provider: BTreeMap<String, ProviderBillingTotal>,
}

impl BillingAggregate {
    pub fn attested_actual_billed_usd(&self) -> Option<Decimal> {
        self.actual_billed_usd
    }
}

fn ensure_unique_id(ids: &mut BTreeSet<String>, id: &str) -> Result<(), BillingError> {
    if !ids.insert(id.to_string()) {
        return Err(BillingError::InvalidEvidence(format!(
            "billing evidence id '{id}' is duplicated"
        )));
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.chars().count() > MAX_ID_CHARS {
        return Err(format!(
            "id must contain between 1 and {MAX_ID_CHARS} characters"
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "billing evidence id '{value}' must use only ASCII letters, digits, '.', '_' or '-'"
        ));
    }
    Ok(())
}

fn validate_provider(value: &str) -> Result<(), String> {
    if value.is_empty() || value.chars().count() > MAX_PROVIDER_CHARS {
        return Err(format!(
            "provider must contain between 1 and {MAX_PROVIDER_CHARS} characters"
        ));
    }
    if value != value.trim() || value != value.to_ascii_lowercase() {
        return Err(format!(
            "provider '{value}' must be trimmed lowercase ASCII"
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "provider '{value}' must use only ASCII letters, digits, '.', '_' or '-'"
        ));
    }
    Ok(())
}

fn normalize_provider_for_query(value: &str) -> Result<String, BillingError> {
    let normalized = value.trim().to_ascii_lowercase();
    validate_provider(&normalized).map_err(BillingError::InvalidQuery)?;
    Ok(normalized)
}

fn validate_source_note(value: &str) -> Result<(), String> {
    let length = value.chars().count();
    if value.trim() != value || length == 0 || length > MAX_SOURCE_NOTE_CHARS {
        return Err(format!(
            "source_note must be trimmed and contain 1 to {MAX_SOURCE_NOTE_CHARS} characters"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err("source_note cannot contain control characters".to_string());
    }
    Ok(())
}

fn checked_add(target: &mut Decimal, amount: Decimal) -> Result<(), BillingError> {
    *target = target
        .checked_add(amount)
        .ok_or(BillingError::ArithmeticOverflow)?;
    Ok(())
}

mod decimal_string {
    use std::str::FromStr;

    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Decimal, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Decimal, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Decimal::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

mod optional_decimal_string {
    use std::str::FromStr;

    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<Decimal>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(value) => serializer.serialize_some(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Decimal>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)?
            .map(|raw| Decimal::from_str(&raw).map_err(serde::de::Error::custom))
            .transpose()
    }
}

mod decimal_map {
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::BillingCategory;

    pub fn serialize<S>(
        values: &BTreeMap<BillingCategory, Decimal>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let strings = values
            .iter()
            .map(|(key, value)| (*key, value.to_string()))
            .collect::<BTreeMap<_, _>>();
        strings.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<BillingCategory, Decimal>, D::Error>
    where
        D: Deserializer<'de>,
    {
        BTreeMap::<BillingCategory, String>::deserialize(deserializer)?
            .into_iter()
            .map(|(key, raw)| {
                Decimal::from_str(&raw)
                    .map(|value| (key, value))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

fn add_months_anchored(anchor: DateTime<Utc>, months: u32) -> Option<DateTime<Utc>> {
    let anchor_month_index = i64::from(anchor.year())
        .checked_mul(12)?
        .checked_add(i64::from(anchor.month0()))?;
    let target_month_index = anchor_month_index.checked_add(i64::from(months))?;
    let target_year = i32::try_from(target_month_index.div_euclid(12)).ok()?;
    let target_month = u32::try_from(target_month_index.rem_euclid(12) + 1).ok()?;
    let final_day = days_in_month(target_year, target_month)?;
    let day = anchor.day().min(final_day);
    let date = NaiveDate::from_ymd_opt(target_year, target_month, day)?;
    Some(Utc.from_utc_datetime(&date.and_time(anchor.time())))
}

fn days_in_month(year: i32, month: u32) -> Option<u32> {
    let (next_year, next_month) = if month == 12 {
        (year.checked_add(1)?, 1)
    } else {
        (year, month.checked_add(1)?)
    };
    let first_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    Some(first_next.pred_opt()?.day())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use chrono::TimeZone;

    use super::*;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    fn decimal(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn one_time(id: &str, amount: &str, charged_at: DateTime<Utc>) -> OneTimeBillingRecord {
        OneTimeBillingRecord {
            id: id.to_string(),
            provider: "openai".to_string(),
            category: BillingCategory::CreditPurchase,
            amount_usd: decimal(amount),
            charged_at,
            attested_at: charged_at,
            source_note: "checked provider billing page".to_string(),
        }
    }

    fn completeness(
        id: &str,
        provider: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BillingCompletenessAttestation {
        BillingCompletenessAttestation {
            id: id.to_string(),
            provider: provider.to_string(),
            effective_from: start,
            effective_to: end,
            attested_at: end,
            source_note: "reviewed all provider statements for this period".to_string(),
        }
    }

    #[test]
    fn monthly_schedule_preserves_anchor_and_half_open_boundaries() {
        let plan = RecurringPlanCharge {
            id: "chatgpt-plan-q1".to_string(),
            provider: "openai".to_string(),
            amount_usd: decimal("20.00"),
            cadence: BillingCadence::Monthly,
            effective_from: at(2024, 1, 31),
            effective_to: at(2024, 4, 1),
            attested_at: at(2024, 4, 1),
            source_note: "checked card statement".to_string(),
        };

        let all = plan
            .charge_instants_in(&BillingWindow::new(at(2024, 1, 1), at(2024, 4, 1)).unwrap())
            .unwrap();
        assert_eq!(all, vec![at(2024, 1, 31), at(2024, 2, 29), at(2024, 3, 31)]);

        let february_only = plan
            .charge_instants_in(&BillingWindow::new(at(2024, 2, 29), at(2024, 3, 31)).unwrap())
            .unwrap();
        assert_eq!(february_only, vec![at(2024, 2, 29)]);
    }

    #[test]
    fn recorded_cash_is_not_actual_without_complete_attestation() {
        let start = at(2026, 7, 1);
        let end = at(2026, 8, 1);
        let query =
            BillingQuery::for_providers(BillingWindow::new(start, end).unwrap(), ["OpenAI"])
                .unwrap();
        let evidence = BillingEvidence {
            one_time_charges: vec![one_time("credits-july", "25.00", at(2026, 7, 10))],
            ..BillingEvidence::default()
        };

        let partial = evidence.aggregate(&query).unwrap();
        assert_eq!(partial.recorded_cash_usd, decimal("25.00"));
        assert_eq!(partial.actual_billed_usd, None);
        assert_eq!(
            partial.actual_billing_status,
            ActualBillingStatus::IncompleteEvidence
        );

        let complete = BillingEvidence {
            completeness_attestations: vec![completeness(
                "openai-july-complete",
                "openai",
                start,
                end,
            )],
            ..evidence
        }
        .aggregate(&query)
        .unwrap();
        assert_eq!(complete.actual_billed_usd, Some(decimal("25.00")));
        assert_eq!(
            complete.actual_billing_status,
            ActualBillingStatus::AttestedComplete
        );
    }

    #[test]
    fn overlapping_or_temporally_invalid_attestations_are_rejected() {
        let july = completeness("july", "anthropic", at(2026, 7, 1), at(2026, 8, 1));
        let overlap = completeness("overlap", "anthropic", at(2026, 7, 15), at(2026, 8, 15));
        let evidence = BillingEvidence {
            completeness_attestations: vec![july.clone(), overlap],
            ..BillingEvidence::default()
        };
        assert!(matches!(
            evidence.validate(),
            Err(BillingError::InvalidEvidence(message)) if message.contains("overlap")
        ));

        let mut attested_too_early = july;
        attested_too_early.attested_at = at(2026, 7, 31);
        assert!(attested_too_early.validate().is_err());
    }

    #[test]
    fn adjacent_attestations_collectively_cover_a_window() {
        let july = completeness("july", "openai", at(2026, 7, 1), at(2026, 8, 1));
        let august = completeness("august", "openai", at(2026, 8, 1), at(2026, 9, 1));
        let evidence = BillingEvidence {
            completeness_attestations: vec![july, august],
            ..BillingEvidence::default()
        };
        let query = BillingQuery::for_providers(
            BillingWindow::new(at(2026, 7, 1), at(2026, 9, 1)).unwrap(),
            ["openai"],
        )
        .unwrap();
        let total = evidence.aggregate(&query).unwrap();
        assert_eq!(total.actual_billed_usd, Some(Decimal::ZERO));
    }

    #[test]
    fn decimal_addition_never_uses_binary_floating_point() {
        let start = at(2026, 7, 1);
        let end = at(2026, 8, 1);
        let evidence = BillingEvidence {
            one_time_charges: vec![
                one_time("first", "0.1", at(2026, 7, 2)),
                one_time("second", "0.2", at(2026, 7, 3)),
            ],
            completeness_attestations: vec![completeness("july-complete", "openai", start, end)],
            ..BillingEvidence::default()
        };
        let query =
            BillingQuery::for_providers(BillingWindow::new(start, end).unwrap(), ["openai"])
                .unwrap();
        let total = evidence.aggregate(&query).unwrap();
        assert_eq!(total.recorded_cash_usd, decimal("0.3"));
        assert_eq!(total.actual_billed_usd, Some(decimal("0.3")));
    }

    #[test]
    fn toml_requires_and_round_trips_decimal_strings() {
        let record = one_time("credits", "19.9900", at(2026, 7, 10));
        let encoded = toml::to_string(&record).unwrap();
        assert!(encoded.contains("amount_usd = \"19.9900\""));
        let decoded: OneTimeBillingRecord = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, record);

        let numeric = encoded.replace("amount_usd = \"19.9900\"", "amount_usd = 19.99");
        assert!(toml::from_str::<OneTimeBillingRecord>(&numeric).is_err());
    }

    #[test]
    fn open_provider_scope_never_claims_an_actual_total() {
        let window = BillingWindow::new(at(2026, 7, 1), at(2026, 8, 1)).unwrap();
        let evidence = BillingEvidence {
            one_time_charges: vec![one_time("credits", "10", at(2026, 7, 10))],
            completeness_attestations: vec![completeness(
                "complete",
                "openai",
                window.start,
                window.end,
            )],
            ..BillingEvidence::default()
        };
        let aggregate = evidence
            .aggregate(&BillingQuery::all_recorded(window))
            .unwrap();
        assert_eq!(aggregate.recorded_cash_usd, decimal("10"));
        assert_eq!(aggregate.actual_billed_usd, None);
        assert_eq!(
            aggregate.actual_billing_status,
            ActualBillingStatus::OpenProviderScope
        );
    }
}
