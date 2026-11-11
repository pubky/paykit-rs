//! Billing Periods (Paykit spec 8.2, Paykit wire 4.4) and the rates rule of period details
//! (Paykit wire 4.9).
//!
//! Billing Periods follow one rule, the anchored period rule: boundary `k` is `anchor` plus
//! `k * every` units, a fixed number of seconds for minutes, hours, days and weeks, and a
//! calendar step from the original anchor for months and years, clamped to the target
//! month's last day. Period `k` is `[boundary(k), boundary(k+1))`. The same rule gives the
//! periods of an Allowance's anchored limits (Paykit spec 8.3).
//!
//! Times are Unix milliseconds, as everywhere in the Switch's formats. Every computation is
//! checked: a period outside the times the grammar can write fails instead of wrapping.

use pubky_switch_core::time::{rfc3339, MAX_TIME_MS};
use serde_json::{json, Map, Value};

use crate::error::{invalid, Error, Result};
use crate::json::Obj;

const DAY_MS: i128 = 86_400_000;

/// A recurrence's or a period limit's unit (Paykit wire 4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unit {
    /// 60 seconds.
    Minute,
    /// 3,600 seconds.
    Hour,
    /// 86,400 seconds.
    Day,
    /// 7 days.
    Week,
    /// A calendar month.
    Month,
    /// A calendar year.
    Year,
}

impl Unit {
    /// Read a unit (Paykit wire 4.4).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for any other text.
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "minute" => Self::Minute,
            "hour" => Self::Hour,
            "day" => Self::Day,
            "week" => Self::Week,
            "month" => Self::Month,
            "year" => Self::Year,
            _ => {
                return invalid("Paykit wire 4.4: a unit is minute, hour, day, week, month or year")
            }
        })
    }

    /// The unit's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }

    /// The unit's length in milliseconds, for the fixed units minute to week; `None` for
    /// months and years, which are calendar steps.
    pub fn fixed_ms(self) -> Option<u64> {
        match self {
            Self::Minute => Some(60_000),
            Self::Hour => Some(3_600_000),
            Self::Day => Some(86_400_000),
            Self::Week => Some(604_800_000),
            Self::Month | Self::Year => None,
        }
    }
}

/// One Billing Period, `[starts_at, ends_at)` in Unix milliseconds, and its index `k`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BillingPeriod {
    /// The period's index `k` from the anchor; negative before the anchor.
    pub index: i64,
    /// The first instant, `boundary(k)`.
    pub starts_at: u64,
    /// The first instant after it, `boundary(k + 1)`.
    pub ends_at: u64,
}

impl BillingPeriod {
    /// The period's start in Unix seconds, rounded down, as obligation IDs and natural keys
    /// name it (Paykit wire 1.5).
    pub fn start_secs(&self) -> u64 {
        self.starts_at / 1000
    }

    /// The period's length in milliseconds.
    pub fn len_ms(&self) -> u64 {
        // Saturating is safe: a computed period ends after it starts (Paykit wire 4.4).
        self.ends_at.saturating_sub(self.starts_at)
    }

    /// Whether `t` lies in the period.
    pub fn contains(&self, t: u64) -> bool {
        self.starts_at <= t && t < self.ends_at
    }

    /// The period as messages name it: `{ "starts_at": time, "ends_at": time }` (Paykit wire
    /// 4.4).
    pub fn to_json(&self) -> Value {
        json!({ "starts_at": rfc3339(self.starts_at), "ends_at": rfc3339(self.ends_at) })
    }
}

/// A named Billing Period, `{ "starts_at", "ends_at" }`, as a message carries it, before it is
/// checked against a recurrence (Paykit wire 4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeriodRef {
    /// `starts_at`, in Unix milliseconds.
    pub starts_at: u64,
    /// `ends_at`, in Unix milliseconds, after `starts_at`.
    pub ends_at: u64,
}

impl PeriodRef {
    /// Read a named period: two times, `ends_at` after `starts_at` (Paykit wire 4.4).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a member that isn't a time, or an empty range.
    pub fn from_json(value: &Value, ctx: &str) -> Result<Self> {
        let obj = Obj::of(value, ctx, "billing_period")?;
        let starts_at = obj.req_time("starts_at")?;
        let ends_at = obj.req_time("ends_at")?;
        if ends_at <= starts_at {
            return invalid(format!(
                "{ctx}: billing_period: ends_at isn't after starts_at"
            ));
        }
        Ok(Self { starts_at, ends_at })
    }

    /// The period's members, times written with three fraction digits.
    pub fn to_json(&self) -> Value {
        json!({ "starts_at": rfc3339(self.starts_at), "ends_at": rfc3339(self.ends_at) })
    }

    /// The period's start in Unix seconds, rounded down (Paykit wire 1.5).
    pub fn start_secs(&self) -> u64 {
        self.starts_at / 1000
    }
}

impl From<BillingPeriod> for PeriodRef {
    fn from(period: BillingPeriod) -> Self {
        Self {
            starts_at: period.starts_at,
            ends_at: period.ends_at,
        }
    }
}

/// The anchored period rule (Paykit spec 8.2): boundary `k` is `anchor + k * every` units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnchoredRule {
    anchor: u64,
    every: u64,
    unit: Unit,
}

impl AnchoredRule {
    /// A rule the caller supplies, with `every` at least 1.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for `every` of 0.
    pub fn new(anchor: u64, every: u64, unit: Unit) -> Result<Self> {
        Self::read(anchor, every, unit).map_err(Error::into_validation)
    }

    /// A rule read from a document, with `every` at least 1 (Paykit wire 4.4).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for `every` of 0.
    pub(crate) fn read(anchor: u64, every: u64, unit: Unit) -> Result<Self> {
        if every == 0 {
            return invalid("Paykit wire 4.4: every is at least 1");
        }
        Ok(Self {
            anchor,
            every,
            unit,
        })
    }

    /// The anchor, in Unix milliseconds.
    pub fn anchor(&self) -> u64 {
        self.anchor
    }

    /// `every`.
    pub fn every(&self) -> u64 {
        self.every
    }

    /// The unit.
    pub fn unit(&self) -> Unit {
        self.unit
    }

    /// Boundary `k`: fixed seconds for minutes to weeks; for months and years a calendar step
    /// from the original anchor, clamped to the target month's last day (Paykit spec 8.2).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a boundary outside the times the grammar writes.
    pub fn boundary(&self, k: i64) -> Result<u64> {
        let anchor = i128::from(self.anchor);
        let steps = i128::from(k)
            .checked_mul(i128::from(self.every))
            .ok_or_else(out_of_range)?;
        let at = match self.unit.fixed_ms() {
            Some(len) => steps
                .checked_mul(i128::from(len))
                .and_then(|d| anchor.checked_add(d))
                .ok_or_else(out_of_range)?,
            None => {
                let months = if self.unit == Unit::Year {
                    steps.checked_mul(12).ok_or_else(out_of_range)?
                } else {
                    steps
                };
                let days = anchor.div_euclid(DAY_MS);
                let time_of_day = anchor.rem_euclid(DAY_MS);
                let (year, month, day) = civil_from_days(days);
                let total = (year * 12 + i128::from(month) - 1)
                    .checked_add(months)
                    .ok_or_else(out_of_range)?;
                let target_year = total.div_euclid(12);
                // The grammar writes years 1970 to 9999; beyond them nothing can overflow.
                if !(1970..=9999).contains(&target_year) {
                    return Err(out_of_range());
                }
                // In 1..=12.
                let target_month = u32::try_from(total.rem_euclid(12) + 1).unwrap_or(1);
                let target_day = day.min(days_in_month(target_year, target_month));
                days_from_civil(target_year, target_month, target_day)
                    .checked_mul(DAY_MS)
                    .and_then(|d| d.checked_add(time_of_day))
                    .ok_or_else(out_of_range)?
            }
        };
        to_ms(at)
    }

    /// The index `k` of the period containing `t`: `boundary(k) <= t < boundary(k + 1)`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when a boundary near `t` is outside the times the grammar writes.
    pub fn index_at(&self, t: u64) -> Result<i64> {
        let t_i = i128::from(t);
        let anchor = i128::from(self.anchor);
        let every = i128::from(self.every);
        let estimate = match self.unit.fixed_ms() {
            Some(len) => (t_i - anchor).div_euclid(every * i128::from(len)),
            None => {
                let per = if self.unit == Unit::Year {
                    every * 12
                } else {
                    every
                };
                let (ty, tm, _) = civil_from_days(t_i.div_euclid(DAY_MS));
                let (ay, am, _) = civil_from_days(anchor.div_euclid(DAY_MS));
                ((ty * 12 + i128::from(tm)) - (ay * 12 + i128::from(am))).div_euclid(per)
            }
        };
        let mut k = i64::try_from(estimate).map_err(|_| out_of_range())?;
        // The estimate is off by at most one step either way; settle it on the boundaries.
        for _ in 0..4 {
            if self.boundary(k)? > t {
                k = k.checked_sub(1).ok_or_else(out_of_range)?;
            } else if self.boundary(k.checked_add(1).ok_or_else(out_of_range)?)? <= t {
                k = k.checked_add(1).ok_or_else(out_of_range)?;
            } else {
                return Ok(k);
            }
        }
        invalid("Paykit spec 8.2: no period contains the time")
    }

    /// Period `k`.
    ///
    /// # Errors
    ///
    /// As [`AnchoredRule::boundary`].
    pub fn period(&self, k: i64) -> Result<BillingPeriod> {
        Ok(BillingPeriod {
            index: k,
            starts_at: self.boundary(k)?,
            ends_at: self.boundary(k.checked_add(1).ok_or_else(out_of_range)?)?,
        })
    }

    /// The period containing `t`.
    ///
    /// # Errors
    ///
    /// As [`AnchoredRule::index_at`].
    pub fn containing(&self, t: u64) -> Result<BillingPeriod> {
        self.period(self.index_at(t)?)
    }
}

/// A request's recurrence (Paykit wire 4.4): `{ "every", "unit", "starts_at", "anchor",
/// "ends_at" }`, `every` at least 1 and `ends_at`, when set, after `starts_at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Recurrence {
    rule: AnchoredRule,
    starts_at: u64,
    ends_at: Option<u64>,
}

impl Recurrence {
    /// A recurrence the caller supplies.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an `ends_at` not after `starts_at`.
    pub fn new(rule: AnchoredRule, starts_at: u64, ends_at: Option<u64>) -> Result<Self> {
        Self::read(rule, starts_at, ends_at).map_err(Error::into_validation)
    }

    /// A recurrence read from a document (Paykit wire 4.4).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an `ends_at` not after `starts_at`.
    fn read(rule: AnchoredRule, starts_at: u64, ends_at: Option<u64>) -> Result<Self> {
        if ends_at.is_some_and(|e| e <= starts_at) {
            return invalid("Paykit wire 4.4: recurrence: ends_at isn't after starts_at");
        }
        Ok(Self {
            rule,
            starts_at,
            ends_at,
        })
    }

    /// Read a recurrence (Paykit wire 4.4).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member or rule.
    pub fn from_json(value: &Value) -> Result<Self> {
        let obj = Obj::of(value, "Paykit wire 4.4: recurrence", "recurrence")?;
        let rule = AnchoredRule::read(
            obj.req_time("anchor")?,
            obj.req_int("every")?,
            Unit::parse(obj.req_str("unit")?)?,
        )?;
        // Paykit wire 4.4: ends_at is present, a time or null; a missing one isn't read as
        // null.
        if !obj.has("ends_at") {
            return invalid("Paykit wire 4.4: recurrence: ends_at: missing; null for none");
        }
        Self::read(rule, obj.req_time("starts_at")?, obj.opt_time("ends_at")?)
    }

    /// The recurrence's members, `ends_at` always written (`null` when unset).
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("anchor".into(), Value::String(rfc3339(self.rule.anchor)));
        map.insert(
            "ends_at".into(),
            self.ends_at
                .map_or(Value::Null, |e| Value::String(rfc3339(e))),
        );
        map.insert("every".into(), Value::from(self.rule.every));
        map.insert("starts_at".into(), Value::String(rfc3339(self.starts_at)));
        map.insert("unit".into(), Value::String(self.rule.unit.as_str().into()));
        Value::Object(map)
    }

    /// The anchored rule.
    pub fn rule(&self) -> &AnchoredRule {
        &self.rule
    }

    /// `starts_at`.
    pub fn starts_at(&self) -> u64 {
        self.starts_at
    }

    /// `ends_at`, if set.
    pub fn ends_at(&self) -> Option<u64> {
        self.ends_at
    }

    /// The first Billing Period: the one containing `starts_at`.
    ///
    /// # Errors
    ///
    /// As [`AnchoredRule::containing`].
    pub fn first(&self) -> Result<BillingPeriod> {
        self.rule.containing(self.starts_at)
    }

    /// The last Billing Period, whole: the one containing the instant just before `ends_at`;
    /// `None` without `ends_at`.
    ///
    /// # Errors
    ///
    /// As [`AnchoredRule::containing`].
    pub fn last(&self) -> Result<Option<BillingPeriod>> {
        self.ends_at
            // Saturating is safe: ends_at is after starts_at, so at least 1.
            .map(|e| self.rule.containing(e.saturating_sub(1)))
            .transpose()
    }

    fn in_range(&self, k: i64) -> Result<bool> {
        Ok(k >= self.first()?.index && self.last()?.is_none_or(|l| k <= l.index))
    }

    /// The Billing Period at `t`, if `t` lies in one of the request's periods.
    ///
    /// # Errors
    ///
    /// As [`AnchoredRule::containing`].
    pub fn lookup(&self, t: u64) -> Result<Option<BillingPeriod>> {
        let period = self.rule.containing(t)?;
        Ok(self.in_range(period.index)?.then_some(period))
    }

    /// The Billing Period a message names, if it equals one of the request's periods,
    /// compared as instants (Paykit wire 4.4); `None` for anything else, which is rejected.
    ///
    /// # Errors
    ///
    /// As [`AnchoredRule::containing`].
    pub fn named(&self, named: &PeriodRef) -> Result<Option<BillingPeriod>> {
        let period = self.rule.containing(named.starts_at)?;
        Ok((period.starts_at == named.starts_at
            && period.ends_at == named.ends_at
            && self.in_range(period.index)?)
        .then_some(period))
    }

    /// The request's periods from the first, at most `n` of them.
    ///
    /// # Errors
    ///
    /// As [`AnchoredRule::period`].
    pub fn periods(&self, n: usize) -> Result<Vec<BillingPeriod>> {
        let first = self.first()?.index;
        let last = self.last()?.map_or(i64::MAX, |l| l.index);
        let mut out = Vec::new();
        let mut k = first;
        while k <= last && out.len() < n {
            out.push(self.rule.period(k)?);
            k = match k.checked_add(1) {
                Some(next) => next,
                None => break,
            };
        }
        Ok(out)
    }
}

/// The start of period details' rates (Paykit spec 8.2, Paykit wire 4.9): their signed
/// `created_at`, in Unix milliseconds, cut to whole seconds, which is v0.1's `valid_from`.
/// `2026-02-28T00:00:00.999Z` starts at `2026-02-28T00:00:00.000Z`.
pub fn rates_start(created_at: u64) -> u64 {
    created_at - created_at % 1000
}

/// Whether the rates of period details apply to a payment (Paykit wire 4.9): its verified
/// payment time, from the payment method's evidence, is at or after their start
/// ([`rates_start`] of the details' `created_at`) and at or before their `expires_at`, all at
/// millisecond precision. Both sides apply it; a payment to an endpoint of the requested
/// asset is paid 1:1 and isn't bound by it. Each details stays valid until its own
/// `expires_at`, so later details replace none.
pub fn rates_apply(created_at: u64, expires_at: u64, verified_payment_time: u64) -> bool {
    rates_start(created_at) <= verified_payment_time && verified_payment_time <= expires_at
}

/// Whether a payer may pay by the rates of period details (Paykit wire 4.9): only while its
/// device clock and its highest verified service time are both at or after their start
/// ([`rates_start`] of the details' `created_at`) and before `expires_at`, at planning and
/// again before money moves.
pub fn payer_may_use_rates(
    created_at: u64,
    expires_at: u64,
    device_time: u64,
    service_time: u64,
) -> bool {
    let start = rates_start(created_at);
    [device_time, service_time]
        .iter()
        .all(|t| start <= *t && *t < expires_at)
}

/// An evidence time in whole Unix seconds, such as a block timestamp, read as `.000`
/// (Paykit wire 4.9); `None` past the last time the grammar writes.
pub fn evidence_seconds(seconds: u64) -> Option<u64> {
    seconds.checked_mul(1000).filter(|ms| *ms <= MAX_TIME_MS)
}

fn out_of_range() -> Error {
    Error::Invalid("Paykit spec 8.2: a period boundary outside the times the grammar writes".into())
}

fn to_ms(at: i128) -> Result<u64> {
    u64::try_from(at)
        .ok()
        .filter(|ms| *ms <= MAX_TIME_MS)
        .ok_or_else(out_of_range)
}

/// The civil date of a day count from 1970-01-01 (proleptic Gregorian).
fn civil_from_days(days: i128) -> (i128, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i128::from(month <= 2);
    // `day` is in 1..=31 and `month` in 1..=12.
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// The day count from 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(year: i128, month: u32, day: u32) -> i128 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i128::from(month);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + i128::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(year: i128, month: u32) -> u32 {
    match month {
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_switch_core::time::parse_rfc3339;

    fn t(text: &str) -> u64 {
        parse_rfc3339(text).unwrap()
    }

    #[test]
    fn test_periods_recurrence_needs_ends_at_present() {
        let full = serde_json::json!({ "anchor": "2026-01-31T00:00:00.000Z", "every": 1,
            "unit": "month", "starts_at": "2026-01-31T00:00:00.000Z", "ends_at": null });
        let read = Recurrence::from_json(&full).unwrap();
        assert_eq!(read.to_json(), full);
        let mut missing = full;
        missing.as_object_mut().unwrap().remove("ends_at");
        let error = Recurrence::from_json(&missing).unwrap_err().to_string();
        assert!(error.contains("ends_at: missing"), "{error}");
    }

    #[test]
    fn test_periods_civil_round_trip() {
        for days in [-719_468i128, -1, 0, 59, 365, 10_957, 20_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn test_periods_monthly_clamps_to_the_last_day() {
        let rule = AnchoredRule::new(t("2026-01-31T00:00:00.000Z"), 1, Unit::Month).unwrap();
        assert_eq!(rule.boundary(1).unwrap(), t("2026-02-28T00:00:00.000Z"));
        assert_eq!(rule.boundary(2).unwrap(), t("2026-03-31T00:00:00.000Z"));
        assert_eq!(rule.boundary(25).unwrap(), t("2028-02-29T00:00:00.000Z"));
        assert_eq!(rule.boundary(-1).unwrap(), t("2025-12-31T00:00:00.000Z"));
        let p = rule.containing(t("2026-03-30T23:59:59.999Z")).unwrap();
        assert_eq!((p.index, p.starts_at), (1, t("2026-02-28T00:00:00.000Z")));
    }

    #[test]
    fn test_periods_fixed_units_go_before_the_anchor() {
        let rule = AnchoredRule::new(t("2026-01-10T00:00:00.000Z"), 2, Unit::Week).unwrap();
        let p = rule.containing(t("2026-01-01T00:00:00.000Z")).unwrap();
        assert_eq!(p.index, -1);
        assert_eq!(p.starts_at, t("2025-12-27T00:00:00.000Z"));
    }

    #[test]
    fn test_periods_refuse_out_of_range_and_zero_every() {
        assert!(matches!(
            AnchoredRule::new(0, 0, Unit::Day),
            Err(Error::Validation(_))
        ));
        assert!(matches!(
            AnchoredRule::read(0, 0, Unit::Day),
            Err(Error::Invalid(_))
        ));
        let rule = AnchoredRule::new(0, 1, Unit::Day).unwrap();
        assert!(matches!(
            Recurrence::new(rule, 5, Some(5)),
            Err(Error::Validation(_))
        ));
        assert!(matches!(
            Recurrence::from_json(&serde_json::json!({ "anchor": "2026-01-31T00:00:00.000Z",
                "every": 1, "unit": "month", "starts_at": "2026-01-31T00:00:00.000Z",
                "ends_at": "2026-01-31T00:00:00.000Z" })),
            Err(Error::Invalid(_))
        ));
        let rule = AnchoredRule::new(0, 1, Unit::Day).unwrap();
        assert!(rule.boundary(-1).is_err());
        assert!(rule.boundary(i64::MAX).is_err());
        let yearly = AnchoredRule::new(0, u64::MAX, Unit::Year).unwrap();
        assert!(yearly.boundary(i64::MAX).is_err());
    }

    #[test]
    fn test_periods_recurrence_first_last_and_named() {
        let value = json!({ "anchor": "2026-01-01T00:00:00.000Z", "every": 1, "unit": "day",
            "starts_at": "2026-01-01T12:00:00.000Z", "ends_at": "2026-01-03T12:00:00.000Z" });
        let rec = Recurrence::from_json(&value).unwrap();
        assert_eq!(rec.first().unwrap().index, 0);
        assert_eq!(rec.last().unwrap().unwrap().index, 2);
        assert_eq!(rec.periods(10).unwrap().len(), 3);
        assert!(rec.lookup(t("2026-01-04T00:00:00.000Z")).unwrap().is_none());
        let named = PeriodRef {
            starts_at: t("2026-01-02T00:00:00.000Z"),
            ends_at: t("2026-01-03T00:00:00.000Z"),
        };
        assert!(rec.named(&named).unwrap().is_some());
        let wrong = PeriodRef {
            ends_at: t("2026-01-02T12:00:00.000Z"),
            ..named
        };
        assert!(rec.named(&wrong).unwrap().is_none());
        assert_eq!(Recurrence::from_json(&rec.to_json()).unwrap(), rec);
    }

    #[test]
    fn test_periods_recurrence_rejects_bad_members() {
        for bad in [
            json!({ "anchor": "2026-01-01T00:00:00.000Z", "every": 0, "unit": "day",
                "starts_at": "2026-01-01T00:00:00.000Z", "ends_at": null }),
            json!({ "anchor": "2026-01-01T00:00:00.000Z", "every": 1, "unit": "fortnight",
                "starts_at": "2026-01-01T00:00:00.000Z", "ends_at": null }),
            json!({ "anchor": "2026-01-01T00:00:00.000Z", "every": 1, "unit": "day",
                "starts_at": "2026-01-02T00:00:00.000Z", "ends_at": "2026-01-01T00:00:00.000Z" }),
        ] {
            assert!(Recurrence::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_periods_rates_rule() {
        let exp = t("2026-03-01T00:00:00.500Z");
        let created = t("2026-02-28T00:00:00.999Z");
        let start = t("2026-02-28T00:00:00.000Z");
        assert_eq!(rates_start(created), start);
        assert!(rates_apply(created, exp, exp));
        assert!(!rates_apply(created, exp, exp + 1));
        assert!(payer_may_use_rates(created, exp, exp - 1, exp - 1));
        assert!(!payer_may_use_rates(created, exp, exp, exp - 1));
        assert!(!payer_may_use_rates(created, exp, exp - 1, exp));
        assert_eq!(
            evidence_seconds(1_772_323_200),
            Some(t("2026-03-01T00:00:00Z"))
        );
        assert_eq!(evidence_seconds(u64::MAX), None);
    }

    #[test]
    fn test_periods_rates_apply_from_created_at_cut_to_seconds() {
        let exp = t("2026-03-01T00:00:00.500Z");
        let created = t("2026-02-28T00:00:00.999Z");
        let start = t("2026-02-28T00:00:00.000Z");
        // From the start, before created_at's own instant: v0.1's valid_from.
        assert!(rates_apply(created, exp, start));
        assert!(!rates_apply(created, exp, start - 1));
        // The payer's two clocks are both held to the start too.
        assert!(payer_may_use_rates(created, exp, start, start));
        assert!(!payer_may_use_rates(created, exp, start - 1, start));
        assert!(!payer_may_use_rates(created, exp, start, start - 1));
    }

    #[test]
    fn test_periods_period_ref_needs_a_range() {
        assert!(PeriodRef::from_json(
            &json!({ "starts_at": "2026-01-02T00:00:00Z", "ends_at": "2026-01-01T00:00:00Z" }),
            "ctx"
        )
        .is_err());
        assert!(PeriodRef::from_json(&json!({ "starts_at": "x", "ends_at": "y" }), "ctx").is_err());
    }
}
