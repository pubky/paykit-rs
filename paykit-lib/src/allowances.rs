//! Allowance terms, ledgers and usage (Paykit spec 8.3; Paykit wire 4.15, 5.5).
//!
//! Allowances are built entirely in apps, on claims and shared records. This module holds the
//! pure rules: the terms and their validity, the ledger value, how each entry counts by its
//! outcome, usage against every limit, the order in which limits refuse a payment, the
//! window of an executor batch, compaction, and the rule for accepting a newer ledger
//! version.
//!
//! Security: usage never reads a ledger as smaller than it is. A ledger that isn't valid is
//! refused, every sum is checked and fails closed on overflow, and a newer version that
//! counts less than the one held is refused. Paykit coordinates the reservation; enforcing
//! the limits is the executor's job (Paykit spec 8.3).

use std::collections::HashMap;

use pubky_switch_core::enc::{b64, pubky_id};
use pubky_switch_core::time::rfc3339;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::amount::{Asset, Decimal, PaymentAmount};
use crate::bodies::identifiers_json;
use crate::error::{ensure, invalid, Error, Result};
use crate::identifiers::{AllowanceId, AttemptId, PaymentEndpointIdentifier};
use crate::json::{bad, check_v1, parse_object, Obj};
use crate::periods::{AnchoredRule, Unit};

/// The window an executor batch is dated in: its entry's `at` and at most 60 s after
/// (Paykit spec 8.3).
pub const ENTRY_WINDOW_MS: u64 = 60_000;

const TERMS: &str = "Paykit wire 4.15: terms";

/// The per-payment range of Allowance terms (Paykit wire 4.15): inclusive, `minimum` not
/// above `maximum`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerPaymentRange {
    /// The smallest payment allowed.
    pub minimum: Decimal,
    /// The largest payment allowed.
    pub maximum: Decimal,
}

/// The period of a limit (Paykit wire 4.15).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LimitPeriod {
    /// `{ "kind": "anchored", "every", "unit", "anchor" }`: the period containing the
    /// evaluation time, by the anchored rule.
    Anchored(AnchoredRule),
    /// `{ "kind": "rolling", "every", "unit" }`, unit minute to week: the last `every` units.
    Rolling {
        /// `every`, at least 1.
        every: u64,
        /// The unit, a fixed one.
        unit: Unit,
    },
}

impl LimitPeriod {
    /// A rolling period's length in milliseconds.
    fn rolling_len(every: u64, unit: Unit) -> Result<u64> {
        unit.fixed_ms()
            .and_then(|len| len.checked_mul(every))
            .ok_or_else(|| Error::Invalid(format!("{TERMS}: a rolling period's length")))
    }
}

/// A period limit (Paykit wire 4.15): an amount limit, a payment count limit or both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeriodLimit {
    /// The most the period may count, inclusive.
    pub amount_limit: Option<Decimal>,
    /// The most payments the period may count, inclusive.
    pub payment_count_limit: Option<u64>,
    /// The period.
    pub period: LimitPeriod,
}

/// Allowance terms (Paykit wire 4.15, `specs/allowances.md`): every member present, `null`
/// where the table allows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowanceTerms {
    /// The asset; a request in another asset is never paid under the Allowance.
    pub asset: Asset,
    /// The inclusive per-payment range.
    pub per_payment_amount: Option<PerPaymentRange>,
    /// The period limits, unique.
    pub period_limits: Vec<PeriodLimit>,
    /// The lifetime amount limit, inclusive.
    pub lifetime_amount_limit: Option<Decimal>,
    /// The first eligible instant.
    pub active_from: Option<u64>,
    /// The first ineligible instant, after `active_from`.
    pub expires_at: Option<u64>,
    /// The only identifiers an executor may pay to, when set: non-empty, unique.
    pub allowed_payment_endpoint_identifiers: Option<Vec<PaymentEndpointIdentifier>>,
}

/// The members of Allowance terms (Paykit wire 4.15), all required.
const TERMS_MEMBERS: [&str; 7] = [
    "asset",
    "per_payment_amount",
    "period_limits",
    "lifetime_amount_limit",
    "active_from",
    "expires_at",
    "allowed_payment_endpoint_identifiers",
];

impl AllowanceTerms {
    /// Read and check terms (Paykit wire 4.15). A time that doesn't parse makes the terms
    /// invalid; it is never read as no limit. Unknown members are ignored (Switch wire 1.1).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member or rule.
    pub fn from_json(value: &Value) -> Result<Self> {
        let obj = Obj::of(value, TERMS, "terms")?;
        for member in TERMS_MEMBERS {
            if !obj.has(member) {
                return Err(bad(TERMS, member, "missing; every member is present"));
            }
        }
        let per_payment_amount = obj
            .opt_obj("per_payment_amount")?
            .map(|p| {
                let range = PerPaymentRange {
                    minimum: Decimal::parse(p.req_str("minimum")?)?,
                    maximum: Decimal::parse(p.req_str("maximum")?)?,
                };
                ensure(
                    range.minimum <= range.maximum,
                    "Paykit wire 4.15: per_payment_amount: minimum isn't above maximum",
                )?;
                Ok::<_, Error>(range)
            })
            .transpose()?;
        let mut period_limits: Vec<PeriodLimit> = Vec::new();
        for item in obj.req_arr("period_limits")? {
            let limit = read_period_limit(item)?;
            if period_limits.contains(&limit) {
                return Err(bad(TERMS, "period_limits", "an entry twice"));
            }
            period_limits.push(limit);
        }
        let active_from = obj.opt_time("active_from")?;
        let expires_at = obj.opt_time("expires_at")?;
        if let (Some(from), Some(until)) = (active_from, expires_at) {
            ensure(
                until > from,
                "Paykit wire 4.15: expires_at is after active_from",
            )?;
        }
        let allowed = obj
            .opt_arr("allowed_payment_endpoint_identifiers")?
            .map(|items| {
                ensure(
                    !items.is_empty(),
                    "Paykit wire 4.15: allowed_payment_endpoint_identifiers is non-empty",
                )?;
                let mut out: Vec<PaymentEndpointIdentifier> = Vec::with_capacity(items.len());
                for item in items {
                    let id = PaymentEndpointIdentifier::parse(item.as_str().ok_or_else(|| {
                        bad(
                            TERMS,
                            "allowed_payment_endpoint_identifiers",
                            "not a string",
                        )
                    })?)?;
                    if out.contains(&id) {
                        return Err(bad(
                            TERMS,
                            "allowed_payment_endpoint_identifiers",
                            "an identifier twice",
                        ));
                    }
                    out.push(id);
                }
                Ok(out)
            })
            .transpose()?;
        let terms = Self {
            asset: Asset::parse(obj.req_str("asset")?)?,
            per_payment_amount,
            period_limits,
            lifetime_amount_limit: obj
                .opt_str("lifetime_amount_limit")?
                .map(Decimal::parse)
                .transpose()?,
            active_from,
            expires_at,
            allowed_payment_endpoint_identifiers: allowed,
        };
        let ceiling = terms.per_payment_amount.is_some()
            || terms.lifetime_amount_limit.is_some()
            || terms.period_limits.iter().any(|l| l.amount_limit.is_some());
        ensure(
            ceiling || terms.expires_at.is_some(),
            "Paykit wire 4.15: terms configure a monetary ceiling or an expiry",
        )?;
        Ok(terms)
    }

    /// Write the terms, every member, `null` where unset.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        let time = |t: Option<u64>| t.map_or(Value::Null, |t| Value::String(rfc3339(t)));
        map.insert("active_from".into(), time(self.active_from));
        map.insert(
            "allowed_payment_endpoint_identifiers".into(),
            self.allowed_payment_endpoint_identifiers
                .as_ref()
                .map_or(Value::Null, |ids| identifiers_json(ids)),
        );
        map.insert("asset".into(), Value::String(self.asset.as_str().into()));
        map.insert("expires_at".into(), time(self.expires_at));
        map.insert(
            "lifetime_amount_limit".into(),
            decimal_or_null(self.lifetime_amount_limit.as_ref()),
        );
        map.insert(
            "per_payment_amount".into(),
            self.per_payment_amount.as_ref().map_or(Value::Null, |r| {
                let mut m = Map::new();
                m.insert("maximum".into(), Value::String(r.maximum.as_str().into()));
                m.insert("minimum".into(), Value::String(r.minimum.as_str().into()));
                Value::Object(m)
            }),
        );
        map.insert(
            "period_limits".into(),
            Value::Array(self.period_limits.iter().map(period_limit_json).collect()),
        );
        Value::Object(map)
    }

    /// The checks an executor makes on a payment before the limits (Paykit spec 8.3, Paykit
    /// wire 4.15, 5.5): the amount is in the terms' asset, refused first; the batch's
    /// service time `t` is at or after `active_from` and before `expires_at`; and the
    /// identifier paid to is allowed.
    ///
    /// # Errors
    ///
    /// The [`TermsRefusal`] that refuses it.
    pub fn check_payment(
        &self,
        amount: &PaymentAmount,
        endpoint: &PaymentEndpointIdentifier,
        t: u64,
    ) -> std::result::Result<(), TermsRefusal> {
        if amount.asset != self.asset {
            return Err(TermsRefusal::Asset);
        }
        if self.active_from.is_some_and(|from| t < from) || self.expires_at.is_some_and(|e| t >= e)
        {
            return Err(TermsRefusal::NotActive);
        }
        if self
            .allowed_payment_endpoint_identifiers
            .as_ref()
            .is_some_and(|ids| !ids.contains(endpoint))
        {
            return Err(TermsRefusal::Endpoint);
        }
        Ok(())
    }

    /// The longest period of the limits at `now` (Paykit wire 5.5 "Compaction"): an
    /// anchored limit's period containing `now`, a rolling limit's length; 0 without limits.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a period outside the times the grammar writes.
    pub fn longest_period(&self, now: u64) -> Result<u64> {
        let mut longest = 0u64;
        for limit in &self.period_limits {
            let len = match &limit.period {
                LimitPeriod::Anchored(rule) => rule.containing(now)?.len_ms(),
                LimitPeriod::Rolling { every, unit } => LimitPeriod::rolling_len(*every, *unit)?,
            };
            longest = longest.max(len);
        }
        Ok(longest)
    }

    /// The `end` of an executor batch's `server_time_in(at, end)` condition (Paykit spec 8.3,
    /// Paykit wire 5.5): `at + 60 s`, cut at the next boundary of any anchored limit, so the
    /// entry lies in exactly one period of every anchored limit.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a period outside the times the grammar writes.
    pub fn entry_window_end(&self, at: u64) -> Result<u64> {
        let mut end = at.saturating_add(ENTRY_WINDOW_MS);
        for limit in &self.period_limits {
            if let LimitPeriod::Anchored(rule) = &limit.period {
                end = end.min(rule.containing(at)?.ends_at);
            }
        }
        Ok(end)
    }
}

/// Why Allowance terms refuse a payment before its limits (Paykit wire 4.15, 5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermsRefusal {
    /// The request is in another asset than the terms'.
    Asset,
    /// The time is before `active_from` or at or after `expires_at`.
    NotActive,
    /// The identifier isn't among `allowed_payment_endpoint_identifiers`.
    Endpoint,
}

fn decimal_or_null(d: Option<&Decimal>) -> Value {
    d.map_or(Value::Null, |d| Value::String(d.as_str().into()))
}

fn read_period_limit(item: &Value) -> Result<PeriodLimit> {
    let limit = Obj::of(item, TERMS, "period_limits")?;
    let period = limit.req_obj("period")?;
    let every = period.req_int("every")?;
    let unit = Unit::parse(period.req_str("unit")?)?;
    let period = match period.req_str("kind")? {
        "anchored" => {
            LimitPeriod::Anchored(AnchoredRule::new(period.req_time("anchor")?, every, unit)?)
        }
        "rolling" => {
            ensure(every >= 1, "Paykit wire 4.15: every is at least 1")?;
            LimitPeriod::rolling_len(every, unit)?;
            LimitPeriod::Rolling { every, unit }
        }
        _ => return Err(bad(TERMS, "period", "kind is anchored or rolling")),
    };
    let out = PeriodLimit {
        amount_limit: limit
            .opt_str("amount_limit")?
            .map(Decimal::parse)
            .transpose()?,
        payment_count_limit: limit.opt_int("payment_count_limit")?,
        period,
    };
    ensure(
        out.amount_limit.is_some() || out.payment_count_limit.is_some(),
        "Paykit wire 4.15: a period limit has an amount limit, a count limit or both",
    )?;
    Ok(out)
}

fn period_limit_json(limit: &PeriodLimit) -> Value {
    let mut period = Map::new();
    match &limit.period {
        LimitPeriod::Anchored(rule) => {
            period.insert("anchor".into(), Value::String(rfc3339(rule.anchor())));
            period.insert("every".into(), Value::from(rule.every()));
            period.insert("kind".into(), Value::String("anchored".into()));
            period.insert("unit".into(), Value::String(rule.unit().as_str().into()));
        }
        LimitPeriod::Rolling { every, unit } => {
            period.insert("every".into(), Value::from(*every));
            period.insert("kind".into(), Value::String("rolling".into()));
            period.insert("unit".into(), Value::String(unit.as_str().into()));
        }
    }
    let mut map = Map::new();
    map.insert(
        "amount_limit".into(),
        decimal_or_null(limit.amount_limit.as_ref()),
    );
    map.insert(
        "payment_count_limit".into(),
        limit.payment_count_limit.map_or(Value::Null, Value::from),
    );
    map.insert("period".into(), Value::Object(period));
    Value::Object(map)
}

/// The outcome a ledger entry records (Paykit wire 5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryOutcome {
    /// `done`: final.
    Done,
    /// `failed`: written only when the attempt's owner failed it or the user released the
    /// amount; a later `done` may replace it.
    Failed,
}

impl EntryOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// One ledger entry: an attempt made under the Allowance (Paykit wire 5.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    /// The request's obligation ID, the claim key of its claim.
    pub obligation: [u8; 32],
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The amount paid; fees aren't counted.
    pub value: Decimal,
    /// The asset.
    pub asset: Asset,
    /// The time the executor stated, vouched for by `server_time_in(at, at + 60 s)`.
    pub at: u64,
    /// The attempt's outcome, once known.
    pub outcome: Option<EntryOutcome>,
}

/// A compacted `failed` entry kept in `totals.failed` (Paykit wire 5.5), so a later `done`
/// still counts once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactedFailed {
    /// The obligation of its claim.
    pub obligation: [u8; 32],
    /// The attempt.
    pub attempt_id: AttemptId,
    /// Its value.
    pub value: Decimal,
}

/// An `allowance_ledgers` record value (Paykit wire 5.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ledger {
    /// The allowee.
    pub counterparty: [u8; 32],
    /// The Allowance.
    pub allowance_id: AllowanceId,
    /// One entry per attempt under the Allowance.
    pub entries: Vec<LedgerEntry>,
    /// `totals.spent`: paid amounts of compacted entries; `None` for `""`, a ledger never
    /// compacted, which reads as zero.
    pub spent: Option<Decimal>,
    /// `totals.count`: payments of compacted entries.
    pub count: u64,
    /// `totals.failed`: compacted `failed` entries.
    pub failed: Vec<CompactedFailed>,
}

const LEDGER: &str = "Paykit wire 5.5: allowance_ledgers";

#[derive(Serialize)]
struct LedgerOut<'a> {
    v: u64,
    counterparty: String,
    allowance_id: &'a str,
    entries: Vec<EntryOut>,
    totals: TotalsOut,
}

#[derive(Serialize)]
struct EntryOut {
    obligation: String,
    attempt_id: String,
    value: String,
    asset: String,
    at: String,
    outcome: Option<&'static str>,
}

#[derive(Serialize)]
struct TotalsOut {
    spent: String,
    count: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failed: Vec<FailedOut>,
}

#[derive(Serialize)]
struct FailedOut {
    obligation: String,
    attempt_id: String,
    value: String,
}

impl Ledger {
    /// An empty ledger, as an executor writes it right after an Allowance's first grant
    /// (Paykit wire 5.5 "A grant implies a ledger").
    pub fn empty(counterparty: [u8; 32], allowance_id: AllowanceId) -> Self {
        Self {
            counterparty,
            allowance_id,
            entries: Vec::new(),
            spent: None,
            count: 0,
            failed: Vec::new(),
        }
    }

    /// Read a ledger value (Paykit wire 5.5). Each `attempt_id` appears once among the
    /// entries and `totals.failed`, and every value, time and `totals.spent` parses.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member or rule.
    pub fn from_json(json: &[u8]) -> Result<Self> {
        let map = parse_object(json, LEDGER)?;
        Self::from_map(&map)
    }

    /// Read a ledger value already parsed as JSON.
    ///
    /// # Errors
    ///
    /// As [`Ledger::from_json`].
    pub fn from_map(map: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(map, LEDGER);
        check_v1(&obj)?;
        let mut entries = Vec::new();
        for item in obj.opt_arr("entries")?.into_iter().flatten() {
            let e = Obj::of(item, LEDGER, "entries")?;
            let outcome = match e.opt_str("outcome")? {
                None => None,
                Some("done") => Some(EntryOutcome::Done),
                Some("failed") => Some(EntryOutcome::Failed),
                Some(_) => return Err(bad(LEDGER, "outcome", "is done or failed")),
            };
            entries.push(LedgerEntry {
                obligation: e.req_b64::<32>("obligation")?,
                attempt_id: AttemptId::from_bytes(e.req_b64::<16>("attempt_id")?),
                value: Decimal::parse(e.req_str("value")?)?,
                asset: Asset::parse(e.req_str("asset")?)?,
                at: e.req_time("at")?,
                outcome,
            });
        }
        let (spent, count, failed) = match obj.opt_obj("totals")? {
            None => (None, 0, Vec::new()),
            Some(t) => {
                let spent = match t.req_str("spent")? {
                    "" => None,
                    text => Some(Decimal::parse(text)?),
                };
                let mut failed = Vec::new();
                for item in t.opt_arr("failed")?.into_iter().flatten() {
                    let f = Obj::of(item, LEDGER, "totals.failed")?;
                    failed.push(CompactedFailed {
                        obligation: f.req_b64::<32>("obligation")?,
                        attempt_id: AttemptId::from_bytes(f.req_b64::<16>("attempt_id")?),
                        value: Decimal::parse(f.req_str("value")?)?,
                    });
                }
                (spent, t.req_int("count")?, failed)
            }
        };
        let ids: Vec<AttemptId> = entries
            .iter()
            .map(|e| e.attempt_id)
            .chain(failed.iter().map(|f| f.attempt_id))
            .collect();
        for (i, id) in ids.iter().enumerate() {
            if ids.iter().take(i).any(|x| x == id) {
                return invalid(
                    "Paykit wire 5.5: an attempt listed twice among the entries and totals.failed",
                );
            }
        }
        Ok(Self {
            counterparty: obj.req_pubky("counterparty")?,
            allowance_id: AllowanceId::parse(obj.req_str("allowance_id")?)?,
            entries,
            spent,
            count,
            failed,
        })
    }

    /// Write the ledger value: every entry with its `outcome` (`null` while unknown), and
    /// `totals` with `spent` `""` for a ledger never compacted and `failed` left out when
    /// empty.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if the value can't be written.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        let out = LedgerOut {
            v: 1,
            counterparty: pubky_id(&self.counterparty),
            allowance_id: self.allowance_id.as_str(),
            entries: self
                .entries
                .iter()
                .map(|e| EntryOut {
                    obligation: b64(&e.obligation),
                    attempt_id: e.attempt_id.to_text(),
                    value: e.value.as_str().into(),
                    asset: e.asset.as_str().into(),
                    at: rfc3339(e.at),
                    outcome: e.outcome.map(EntryOutcome::as_str),
                })
                .collect(),
            totals: TotalsOut {
                spent: self
                    .spent
                    .as_ref()
                    .map_or_else(String::new, |s| s.as_str().into()),
                count: self.count,
                failed: self
                    .failed
                    .iter()
                    .map(|f| FailedOut {
                        obligation: b64(&f.obligation),
                        attempt_id: f.attempt_id.to_text(),
                        value: f.value.as_str().into(),
                    })
                    .collect(),
            },
        };
        serde_json::to_vec(&out).map_err(|_| Error::Invalid(format!("{LEDGER}: can't be written")))
    }

    /// "A valid ledger" against the terms (Paykit wire 5.5): every entry's asset is the
    /// terms' asset. Usage and compaction refuse a ledger that isn't valid.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an entry of another asset.
    pub fn check_valid(&self, terms: &AllowanceTerms) -> Result<()> {
        ensure(
            self.entries.iter().all(|e| e.asset == terms.asset),
            "Paykit wire 5.5: a ledger entry in another asset than the terms'",
        )
    }

    fn spent_scaled(&self) -> u128 {
        self.spent.as_ref().map_or(0, Decimal::scaled)
    }
}

/// What an attempt's claim shows (Paykit wire 5.5 "Counting", Switch wire 6.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimState {
    /// The attempt is open.
    Open,
    /// The attempt is `unknown`.
    Unknown,
    /// The attempt is `done`.
    Done,
    /// The attempt `failed`; `by_owner` when its `resolved_by` is the attempt's owner.
    Failed {
        /// Whether the attempt's owner wrote it.
        by_owner: bool,
    },
}

/// How an entry counts (Paykit spec 8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counting {
    /// Held against the limits until its outcome is known.
    Reserved,
    /// Paid.
    Spent,
    /// Freed.
    Released,
}

/// How an entry counts (Paykit spec 8.3, Paykit wire 5.5): its own `done` spends; its own
/// `failed` releases unless its claim now shows `done`, which spends. Without an `outcome`,
/// by its claim: `open` and `unknown` reserve, `done` spends, `failed` releases only when the
/// attempt's owner wrote it and otherwise reserves until the user releases it, and an entry
/// whose claim is gone, or doesn't hold the attempt (`None`), spends.
pub fn counts(entry: &LedgerEntry, claim: Option<ClaimState>) -> Counting {
    match (entry.outcome, claim) {
        (Some(EntryOutcome::Done), _) => Counting::Spent,
        (Some(EntryOutcome::Failed), Some(ClaimState::Done)) => Counting::Spent,
        (Some(EntryOutcome::Failed), _) => Counting::Released,
        (None, None) => Counting::Spent,
        (None, Some(ClaimState::Open | ClaimState::Unknown)) => Counting::Reserved,
        (None, Some(ClaimState::Done)) => Counting::Spent,
        (None, Some(ClaimState::Failed { by_owner: true })) => Counting::Released,
        (None, Some(ClaimState::Failed { by_owner: false })) => Counting::Reserved,
    }
}

/// One period limit's usage: the amount and count of the entries it counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeriodUsage {
    /// The amount counted.
    pub amount: Decimal,
    /// The payments counted.
    pub count: u64,
}

/// Usage of an Allowance at one time (Paykit spec 8.3, Paykit wire 5.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    /// The lifetime amount: compacted totals, reserved and spent entries, and compacted
    /// failed attempts whose claim now shows `done`.
    pub lifetime: Decimal,
    /// Each period limit's usage, in the order of `period_limits`.
    pub periods: Vec<PeriodUsage>,
    /// Entries reserved.
    pub reserved: u64,
    /// Entries spent.
    pub spent: u64,
    /// Entries released.
    pub released: u64,
}

fn overflow() -> Error {
    Error::Invalid("Paykit wire 1.2: an Allowance sum overflows; usage fails closed".into())
}

fn add(a: u128, b: u128) -> Result<u128> {
    a.checked_add(b).ok_or_else(overflow)
}

fn in_limit(period: &LimitPeriod, entry_at: u64, t: u64) -> Result<bool> {
    Ok(match period {
        LimitPeriod::Anchored(rule) => rule.containing(t)?.contains(entry_at),
        LimitPeriod::Rolling { every, unit } => {
            let len = LimitPeriod::rolling_len(*every, *unit)?;
            // An entry counts while `t < at + 60 s + L`, with no lower bound.
            entry_at
                .checked_add(ENTRY_WINDOW_MS)
                .and_then(|x| x.checked_add(len))
                .is_none_or(|until| t < until)
        }
    })
}

/// Usage at time `t` (Paykit spec 8.3, Paykit wire 5.5): every entry counted by
/// [`counts`] with its attempt's claim from `claims`, for every period limit and the lifetime
/// amount. A `totals.failed` item counts like an entry's own `failed`, in the lifetime amount
/// only: it adds its value while its claim shows `done`.
///
/// # Errors
///
/// [`Error::Invalid`] for a ledger that isn't valid against the terms, or a sum that
/// overflows: usage never reads a ledger as smaller.
pub fn usage(
    terms: &AllowanceTerms,
    ledger: &Ledger,
    claims: &HashMap<AttemptId, ClaimState>,
    t: u64,
) -> Result<Usage> {
    ledger.check_valid(terms)?;
    let mut lifetime = ledger.spent_scaled();
    let mut periods = vec![(0u128, 0u64); terms.period_limits.len()];
    let (mut reserved, mut spent, mut released) = (0u64, 0u64, 0u64);
    for item in &ledger.failed {
        if claims.get(&item.attempt_id) == Some(&ClaimState::Done) {
            lifetime = add(lifetime, item.value.scaled())?;
        }
    }
    for entry in &ledger.entries {
        match counts(entry, claims.get(&entry.attempt_id).copied()) {
            Counting::Reserved => reserved = reserved.saturating_add(1),
            Counting::Spent => spent = spent.saturating_add(1),
            Counting::Released => {
                released = released.saturating_add(1);
                continue;
            }
        }
        lifetime = add(lifetime, entry.value.scaled())?;
        for (limit, usage) in terms.period_limits.iter().zip(periods.iter_mut()) {
            if in_limit(&limit.period, entry.at, t)? {
                usage.0 = add(usage.0, entry.value.scaled())?;
                usage.1 = usage.1.checked_add(1).ok_or_else(overflow)?;
            }
        }
    }
    Ok(Usage {
        lifetime: Decimal::from_scaled(lifetime),
        periods: periods
            .into_iter()
            .map(|(amount, count)| PeriodUsage {
                amount: Decimal::from_scaled(amount),
                count,
            })
            .collect(),
        reserved,
        spent,
        released,
    })
}

/// A limit of Allowance terms that refuses a payment (Paykit wire 5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    /// The per-payment range: `per_payment`.
    PerPayment,
    /// A period's amount limit: `period_amount`.
    PeriodAmount,
    /// A period's payment count limit: `period_count`.
    PeriodCount,
    /// The lifetime amount limit: `lifetime`.
    Lifetime,
}

impl LimitKind {
    /// The limit's name, as the vectors' `refused_by` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PerPayment => "per_payment",
            Self::PeriodAmount => "period_amount",
            Self::PeriodCount => "period_count",
            Self::Lifetime => "lifetime",
        }
    }
}

/// Check a candidate payment of `value` against every limit (Paykit wire 5.5): limits are
/// inclusive ceilings, checked in order, the per-payment range first, then each period limit
/// in the order of `period_limits`, its amount before its count, then the lifetime limit. A
/// sum that overflows refuses.
///
/// # Errors
///
/// The first [`LimitKind`] that refuses.
pub fn check_limits(
    terms: &AllowanceTerms,
    usage: &Usage,
    value: &Decimal,
) -> std::result::Result<(), LimitKind> {
    if terms
        .per_payment_amount
        .as_ref()
        .is_some_and(|range| value < &range.minimum || value > &range.maximum)
    {
        return Err(LimitKind::PerPayment);
    }
    for (limit, used) in terms.period_limits.iter().zip(&usage.periods) {
        if limit.amount_limit.as_ref().is_some_and(|cap| {
            used.amount
                .scaled()
                .checked_add(value.scaled())
                .is_none_or(|sum| sum > cap.scaled())
        }) {
            return Err(LimitKind::PeriodAmount);
        }
        if limit
            .payment_count_limit
            .is_some_and(|cap| used.count.checked_add(1).is_none_or(|n| n > cap))
        {
            return Err(LimitKind::PeriodCount);
        }
    }
    if terms.lifetime_amount_limit.as_ref().is_some_and(|cap| {
        usage
            .lifetime
            .scaled()
            .checked_add(value.scaled())
            .is_none_or(|sum| sum > cap.scaled())
    }) {
        return Err(LimitKind::Lifetime);
    }
    Ok(())
}

/// Whether an entry is old enough to compact at `now`: `at + longest period + 60 s < now`.
fn old_enough(at: u64, longest: u64, now: u64) -> bool {
    at.checked_add(longest)
        .and_then(|x| x.checked_add(ENTRY_WINDOW_MS))
        .is_some_and(|x| x < now)
}

/// Compact a ledger at `now` (Paykit spec 8.3, Paykit wire 5.5): drop entries whose own
/// outcome is final and whose `at + longest period + 60 s` is before `now`; a `done` one's
/// value and count go to the totals, and a `failed` one is kept in `totals.failed` with its
/// obligation, attempt and value. `spent` is then written in the normalized spelling.
///
/// Returns `None` when nothing is compacted.
///
/// # Errors
///
/// [`Error::Invalid`] for a ledger that isn't valid against the terms, or a sum that
/// overflows.
pub fn compact(terms: &AllowanceTerms, ledger: &Ledger, now: u64) -> Result<Option<Ledger>> {
    ledger.check_valid(terms)?;
    let longest = terms.longest_period(now)?;
    let mut spent = ledger.spent_scaled();
    let mut count = ledger.count;
    let mut failed = ledger.failed.clone();
    let mut kept = Vec::with_capacity(ledger.entries.len());
    for entry in &ledger.entries {
        match entry.outcome {
            Some(outcome) if old_enough(entry.at, longest, now) => match outcome {
                EntryOutcome::Done => {
                    spent = add(spent, entry.value.scaled())?;
                    count = count.checked_add(1).ok_or_else(overflow)?;
                }
                EntryOutcome::Failed => failed.push(CompactedFailed {
                    obligation: entry.obligation,
                    attempt_id: entry.attempt_id,
                    value: entry.value.clone(),
                }),
            },
            _ => kept.push(entry.clone()),
        }
    }
    if kept.len() == ledger.entries.len() {
        return Ok(None);
    }
    Ok(Some(Ledger {
        counterparty: ledger.counterparty,
        allowance_id: ledger.allowance_id.clone(),
        entries: kept,
        spent: Some(Decimal::from_scaled(spent)),
        count,
        failed,
    }))
}

/// "From one version to the next" (Paykit spec 8.3, Paykit wire 5.5): a reader that holds
/// `held` accepts `newer`, written at service time `newer_time`, only if it counts no less.
///
/// Every held entry is still there with the same obligation, attempt, value, asset and time,
/// its outcome unchanged or gone from none to `done` or `failed`, or from `failed` to
/// `done`; or it was compacted, old enough at `newer_time`: a `done` one into the totals, and
/// one without an outcome or `failed` into `totals.failed` with the same obligation and
/// value, or into the totals. Every held `totals.failed` item is still there unchanged or left
/// into the totals, and `spent` and `count` grew by at least what left into them. New
/// entries are allowed.
///
/// # Errors
///
/// [`Error::Invalid`] naming the rule broken: the reader ignores and flags that version.
pub fn check_successor(
    terms: &AllowanceTerms,
    held: &Ledger,
    newer: &Ledger,
    newer_time: u64,
) -> Result<()> {
    held.check_valid(terms)?;
    newer.check_valid(terms)?;
    let refuse = |why: &str| invalid(format!("Paykit wire 5.5: a newer ledger version {why}"));
    let longest = terms.longest_period(newer_time)?;
    let (mut into_spent, mut into_count) = (0u128, 0u64);
    for entry in &held.entries {
        if let Some(n) = newer
            .entries
            .iter()
            .find(|n| n.attempt_id == entry.attempt_id)
        {
            if (n.obligation, &n.value, &n.asset, n.at)
                != (entry.obligation, &entry.value, &entry.asset, entry.at)
            {
                return refuse("changes an entry");
            }
            let allowed = match (entry.outcome, n.outcome) {
                (a, b) if a == b => true,
                (None, Some(_)) | (Some(EntryOutcome::Failed), Some(EntryOutcome::Done)) => true,
                _ => false,
            };
            if !allowed {
                return refuse("moves an entry's outcome back");
            }
            continue;
        }
        if !old_enough(entry.at, longest, newer_time) {
            return refuse("drops an entry before it could be compacted");
        }
        match (
            entry.outcome,
            newer
                .failed
                .iter()
                .find(|f| f.attempt_id == entry.attempt_id),
        ) {
            (Some(EntryOutcome::Done), Some(_)) => {
                return refuse("moves a done entry into totals.failed")
            }
            (_, Some(f)) => {
                if f.obligation != entry.obligation || f.value != entry.value {
                    return refuse("keeps a compacted entry with another obligation or value");
                }
            }
            (_, None) => {
                into_spent = add(into_spent, entry.value.scaled())?;
                into_count = into_count.checked_add(1).ok_or_else(overflow)?;
            }
        }
    }
    for item in &held.failed {
        if newer.failed.contains(item) {
            continue;
        }
        if newer.failed.iter().any(|f| f.attempt_id == item.attempt_id) {
            return refuse("changes a totals.failed item");
        }
        into_spent = add(into_spent, item.value.scaled())?;
        into_count = into_count.checked_add(1).ok_or_else(overflow)?;
    }
    let spent_needed = add(held.spent_scaled(), into_spent)?;
    let count_needed = held.count.checked_add(into_count).ok_or_else(overflow)?;
    if newer.spent_scaled() < spent_needed || newer.count < count_needed {
        return refuse("doesn't grow spent and count by what left into them");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_switch_core::time::parse_rfc3339;
    use serde_json::json;

    fn t(text: &str) -> u64 {
        parse_rfc3339(text).unwrap()
    }

    fn terms() -> Value {
        json!({ "active_from": null, "allowed_payment_endpoint_identifiers": null, "asset": "btc",
            "expires_at": null, "lifetime_amount_limit": "0.10",
            "per_payment_amount": { "maximum": "0.01", "minimum": "0.0001" },
            "period_limits": [{ "amount_limit": "0.03", "payment_count_limit": 3,
                "period": { "anchor": "2026-01-31T00:00:00.000Z", "every": 1, "kind": "anchored", "unit": "month" } }] })
    }

    fn entry(id: u8, value: &str, at: &str, outcome: Option<EntryOutcome>) -> LedgerEntry {
        LedgerEntry {
            obligation: [id; 32],
            attempt_id: AttemptId::from_bytes([id; 16]),
            value: Decimal::parse(value).unwrap(),
            asset: Asset::parse("btc").unwrap(),
            at: t(at),
            outcome,
        }
    }

    fn ledger(entries: Vec<LedgerEntry>) -> Ledger {
        Ledger {
            entries,
            ..Ledger::empty(
                [5; 32],
                AllowanceId::parse("d0efffb7-191b-4709-b6c7-e343af5500b4").unwrap(),
            )
        }
    }

    #[test]
    fn test_terms_round_trip() {
        let parsed = AllowanceTerms::from_json(&terms()).unwrap();
        assert_eq!(parsed.to_json(), terms());
    }

    #[test]
    fn test_terms_reject_each_rule() {
        let cases: Vec<(&str, Value)> = vec![
            (
                "per_payment_amount",
                json!({ "minimum": "2", "maximum": "1" }),
            ),
            ("asset", json!("")),
            ("active_from", json!("not a time")),
            ("allowed_payment_endpoint_identifiers", json!([])),
            ("allowed_payment_endpoint_identifiers", json!(["a", "a"])),
            (
                "period_limits",
                json!([{ "amount_limit": null, "payment_count_limit": null,
                "period": { "kind": "rolling", "every": 1, "unit": "day" } }]),
            ),
            (
                "period_limits",
                json!([{ "amount_limit": "1", "payment_count_limit": null,
                "period": { "kind": "rolling", "every": 1, "unit": "month" } }]),
            ),
            (
                "period_limits",
                json!([{ "amount_limit": "1", "payment_count_limit": null,
                "period": { "kind": "rolling", "every": 0, "unit": "day" } }]),
            ),
        ];
        for (member, value) in cases {
            let mut v = terms();
            v[member] = value;
            assert!(AllowanceTerms::from_json(&v).is_err(), "{member}");
        }
        let mut missing = terms();
        missing.as_object_mut().unwrap().remove("active_from");
        assert!(AllowanceTerms::from_json(&missing).is_err());
        let mut twice = terms();
        let limit = twice["period_limits"][0].clone();
        twice["period_limits"] = json!([limit.clone(), limit]);
        assert!(AllowanceTerms::from_json(&twice).is_err());
        let mut no_ceiling = terms();
        no_ceiling["per_payment_amount"] = Value::Null;
        no_ceiling["lifetime_amount_limit"] = Value::Null;
        no_ceiling["period_limits"][0]["amount_limit"] = Value::Null;
        assert!(AllowanceTerms::from_json(&no_ceiling).is_err());
        no_ceiling["expires_at"] = json!("2027-01-01T00:00:00.000Z");
        assert!(AllowanceTerms::from_json(&no_ceiling).is_ok());
        let mut order = terms();
        order["active_from"] = json!("2027-01-01T00:00:00.000Z");
        order["expires_at"] = json!("2027-01-01T00:00:00.000Z");
        assert!(AllowanceTerms::from_json(&order).is_err());
    }

    #[test]
    fn test_terms_check_payment() {
        let mut v = terms();
        v["allowed_payment_endpoint_identifiers"] = json!(["btc-lightning-bolt11"]);
        v["active_from"] = json!("2026-01-01T00:00:00.000Z");
        v["expires_at"] = json!("2027-01-01T00:00:00.000Z");
        let terms = AllowanceTerms::from_json(&v).unwrap();
        let ln = PaymentEndpointIdentifier::parse("btc-lightning-bolt11").unwrap();
        let btc = PaymentAmount {
            value: Decimal::parse("0.01").unwrap(),
            asset: Asset::parse("btc").unwrap(),
        };
        let now = t("2026-06-01T00:00:00.000Z");
        assert_eq!(terms.check_payment(&btc, &ln, now), Ok(()));
        let usd = PaymentAmount {
            asset: Asset::parse("usd").unwrap(),
            ..btc.clone()
        };
        assert_eq!(
            terms.check_payment(&usd, &ln, now),
            Err(TermsRefusal::Asset)
        );
        assert_eq!(
            terms.check_payment(&btc, &ln, t("2027-01-01T00:00:00.000Z")),
            Err(TermsRefusal::NotActive)
        );
        let other = PaymentEndpointIdentifier::parse("btc-bitcoin-p2wpkh").unwrap();
        assert_eq!(
            terms.check_payment(&btc, &other, now),
            Err(TermsRefusal::Endpoint)
        );
    }

    #[test]
    fn test_counts_follow_the_outcome_table() {
        let open = entry(1, "0.01", "2026-02-01T00:00:00.000Z", None);
        for (claim, want) in [
            (Some(ClaimState::Open), Counting::Reserved),
            (Some(ClaimState::Unknown), Counting::Reserved),
            (Some(ClaimState::Done), Counting::Spent),
            (
                Some(ClaimState::Failed { by_owner: true }),
                Counting::Released,
            ),
            (
                Some(ClaimState::Failed { by_owner: false }),
                Counting::Reserved,
            ),
            (None, Counting::Spent),
        ] {
            assert_eq!(counts(&open, claim), want, "{claim:?}");
        }
        let failed = LedgerEntry {
            outcome: Some(EntryOutcome::Failed),
            ..open
        };
        assert_eq!(counts(&failed, Some(ClaimState::Done)), Counting::Spent);
        assert_eq!(counts(&failed, None), Counting::Released);
    }

    #[test]
    fn test_usage_and_limits_in_order() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let l = ledger(vec![
            entry(
                1,
                "0.01",
                "2026-02-27T00:00:00.000Z",
                Some(EntryOutcome::Done),
            ),
            entry(
                2,
                "0.01",
                "2026-02-28T00:00:00.000Z",
                Some(EntryOutcome::Done),
            ),
        ]);
        let u = usage(&terms, &l, &HashMap::new(), t("2026-03-01T00:00:00.000Z")).unwrap();
        assert_eq!(u.periods[0].count, 1);
        assert_eq!(u.lifetime.as_str(), "0.02");
        let d = |s: &str| Decimal::parse(s).unwrap();
        assert_eq!(check_limits(&terms, &u, &d("0.01")), Ok(()));
        assert_eq!(
            check_limits(&terms, &u, &d("0.02")),
            Err(LimitKind::PerPayment)
        );
        let full = Usage {
            periods: vec![PeriodUsage {
                amount: d("0.025"),
                count: 1,
            }],
            ..u.clone()
        };
        assert_eq!(
            check_limits(&terms, &full, &d("0.01")),
            Err(LimitKind::PeriodAmount)
        );
        let counted = Usage {
            periods: vec![PeriodUsage {
                amount: d("0"),
                count: 3,
            }],
            ..u.clone()
        };
        assert_eq!(
            check_limits(&terms, &counted, &d("0.01")),
            Err(LimitKind::PeriodCount)
        );
        let spent = Usage {
            lifetime: d("0.095"),
            ..u
        };
        assert_eq!(
            check_limits(&terms, &spent, &d("0.01")),
            Err(LimitKind::Lifetime)
        );
    }

    #[test]
    fn test_usage_refuses_an_invalid_ledger() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let mut e = entry(1, "0.01", "2026-02-27T00:00:00.000Z", None);
        e.asset = Asset::parse("usd").unwrap();
        let l = ledger(vec![e]);
        assert!(usage(&terms, &l, &HashMap::new(), 0).is_err());
        assert!(compact(&terms, &l, 0).is_err());
        let mut huge = ledger(vec![entry(1, "1", "2026-02-27T00:00:00.000Z", None)]);
        huge.spent = Some(Decimal::from_scaled(u128::MAX));
        assert!(usage(
            &terms,
            &huge,
            &HashMap::new(),
            t("2026-03-01T00:00:00.000Z")
        )
        .is_err());
    }

    #[test]
    fn test_ledger_round_trips_and_refuses_repeats() {
        let l = ledger(vec![entry(1, "0.01", "2026-02-27T00:00:00.000Z", None)]);
        let json = l.to_json().unwrap();
        assert_eq!(Ledger::from_json(&json).unwrap(), l);
        let mut twice = l.clone();
        twice.failed.push(CompactedFailed {
            obligation: [1; 32],
            attempt_id: AttemptId::from_bytes([1; 16]),
            value: Decimal::zero(),
        });
        assert!(Ledger::from_json(&twice.to_json().unwrap()).is_err());
        assert!(Ledger::from_json(br#"{"v":2,"counterparty":"x","allowance_id":"y"}"#).is_err());
    }

    #[test]
    fn test_compaction_and_successor() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let before = ledger(vec![
            entry(
                1,
                "0.01",
                "2026-01-05T00:00:00.000Z",
                Some(EntryOutcome::Done),
            ),
            entry(
                2,
                "0.02",
                "2026-01-06T00:00:00.000Z",
                Some(EntryOutcome::Failed),
            ),
            entry(3, "0.01", "2026-01-07T00:00:00.000Z", None),
        ]);
        let now = t("2026-03-10T00:00:00.000Z");
        let after = compact(&terms, &before, now).unwrap().unwrap();
        assert_eq!(after.entries.len(), 1);
        assert_eq!(after.spent.as_ref().map(Decimal::as_str), Some("0.01"));
        assert_eq!(after.failed.len(), 1);
        assert!(compact(&terms, &after, now).unwrap().is_none());
        check_successor(&terms, &before, &after, now).unwrap();
        assert!(check_successor(&terms, &before, &after, t("2026-01-10T00:00:00.000Z")).is_err());
        let mut flat = after.clone();
        flat.spent = None;
        assert!(check_successor(&terms, &before, &flat, now).is_err());
        let mut back = before.clone();
        back.entries[0].outcome = Some(EntryOutcome::Failed);
        assert!(check_successor(&terms, &before, &back, now).is_err());
    }

    #[test]
    fn test_entry_window_is_cut_at_a_boundary() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let at = t("2026-02-27T23:59:30.000Z");
        assert_eq!(
            terms.entry_window_end(at).unwrap(),
            t("2026-02-28T00:00:00.000Z")
        );
        let mid = t("2026-02-10T00:00:00.000Z");
        assert_eq!(terms.entry_window_end(mid).unwrap(), mid + ENTRY_WINDOW_MS);
        assert_eq!(terms.longest_period(mid).unwrap(), 28 * 86_400_000);
    }
}
