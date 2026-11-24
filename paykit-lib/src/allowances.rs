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
use pubky_switch_core::time::{rfc3339, MAX_TIME_MS};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::amount::{Asset, Decimal, PaymentAmount};
use crate::bodies::identifiers_json;
use crate::error::{ensure, invalid, Error, Result};
use crate::identifiers::{AllowanceId, AttemptId, PaymentEndpointIdentifier};
use crate::json::{bad, check_v1, parse_object, Obj};
use crate::periods::{AnchoredRule, Unit};
use crate::records::{Member, Shape, UnknownMembers};

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

/// A time of Allowance terms (Paykit wire 4.15): its instant, and its text as spelled.
///
/// Terms are signed in the proposal, and two spellings of one instant are distinct period
/// limits (`specs/allowances.md`), so a received time is written back as it was read. A
/// time built locally takes the canonical spelling, with three fraction digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermsTime {
    ms: u64,
    text: String,
}

impl TermsTime {
    /// A time built locally, in the canonical spelling (Switch wire 1.6).
    pub fn new(ms: u64) -> Self {
        Self {
            ms,
            text: rfc3339(ms),
        }
    }

    /// Read a time as spelled (Switch wire 1.6).
    fn read(text: &str, member: &str) -> Result<Self> {
        Ok(Self {
            ms: crate::json::time(text, TERMS, member)?,
            text: text.to_owned(),
        })
    }

    /// The instant, in Unix milliseconds.
    pub fn ms(&self) -> u64 {
        self.ms
    }

    /// The time as spelled.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

/// An anchored period of a limit (Paykit wire 4.15): its rule, and its `anchor` as spelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchoredLimit {
    rule: AnchoredRule,
    anchor: TermsTime,
}

impl AnchoredLimit {
    /// An anchored period the caller supplies, from `anchor`, `every` and `unit`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for `every` of 0.
    pub fn new(anchor: TermsTime, every: u64, unit: Unit) -> Result<Self> {
        Self::read(anchor, every, unit).map_err(Error::into_validation)
    }

    /// An anchored period read from terms (Paykit wire 4.15).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for `every` of 0.
    fn read(anchor: TermsTime, every: u64, unit: Unit) -> Result<Self> {
        Ok(Self {
            rule: AnchoredRule::read(anchor.ms(), every, unit)?,
            anchor,
        })
    }

    /// The rule.
    pub fn rule(&self) -> &AnchoredRule {
        &self.rule
    }

    /// The anchor, as spelled.
    pub fn anchor(&self) -> &TermsTime {
        &self.anchor
    }
}

/// The period of a limit (Paykit wire 4.15).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LimitPeriod {
    /// `{ "kind": "anchored", "every", "unit", "anchor" }`: the period containing the
    /// evaluation time, by the anchored rule.
    Anchored(AnchoredLimit),
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
    pub active_from: Option<TermsTime>,
    /// The first ineligible instant, after `active_from`.
    pub expires_at: Option<TermsTime>,
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
        let mut spellings: Vec<LimitSpelling<'_>> = Vec::new();
        for item in obj.req_arr("period_limits")? {
            let (limit, spelling) = read_period_limit(item)?;
            if spellings.contains(&spelling) {
                return Err(bad(TERMS, "period_limits", "an entry twice"));
            }
            spellings.push(spelling);
            period_limits.push(limit);
        }
        let active_from = obj
            .opt_str("active_from")?
            .map(|t| TermsTime::read(t, "active_from"))
            .transpose()?;
        let expires_at = obj
            .opt_str("expires_at")?
            .map(|t| TermsTime::read(t, "expires_at"))
            .transpose()?;
        if let (Some(from), Some(until)) = (&active_from, &expires_at) {
            ensure(
                until.ms() > from.ms(),
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

    /// Write the terms, every member, `null` where unset, each amount and time in the
    /// spelling it was read with, or the canonical one for terms built locally, so received
    /// terms write back as they were signed and distinct period limits stay distinct.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for terms a recipient refuses (Paykit wire 4.15): the writer
    /// reads them back by [`AllowanceTerms::from_json`], and refuses terms that fail it or
    /// don't read back as themselves.
    pub fn to_json(&self) -> Result<Value> {
        let value = self.json_unchecked();
        let back = Self::from_json(&value).map_err(Error::into_validation)?;
        if &back != self || back.json_unchecked() != value {
            return Err(Error::Validation(format!(
                "{TERMS}: the terms don't write as themselves"
            )));
        }
        Ok(value)
    }

    /// The terms as written, before the reader checks them.
    pub(crate) fn json_unchecked(&self) -> Value {
        let mut map = Map::new();
        let time = |t: &Option<TermsTime>| {
            t.as_ref()
                .map_or(Value::Null, |t| Value::String(t.as_str().into()))
        };
        map.insert("active_from".into(), time(&self.active_from));
        map.insert(
            "allowed_payment_endpoint_identifiers".into(),
            self.allowed_payment_endpoint_identifiers
                .as_ref()
                .map_or(Value::Null, |ids| identifiers_json(ids)),
        );
        map.insert("asset".into(), Value::String(self.asset.as_str().into()));
        map.insert("expires_at".into(), time(&self.expires_at));
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
        if self.active_from.as_ref().is_some_and(|from| t < from.ms())
            || self.expires_at.as_ref().is_some_and(|e| t >= e.ms())
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
        Ok(self.longest_period_span(now)?.0)
    }

    /// The longest period at `t` ([`AllowanceTerms::longest_period`]) and the first time
    /// after `t` at which it may be another: the end of `t`'s period of every anchored limit
    /// in months or years, the only periods whose length varies; `None` when the terms have
    /// no such limit, so the longest period is the same at every time.
    ///
    /// # Errors
    ///
    /// As [`AllowanceTerms::longest_period`].
    fn longest_period_span(&self, t: u64) -> Result<(u64, Option<u64>)> {
        let mut longest = 0u64;
        let mut changes: Option<u64> = None;
        for limit in &self.period_limits {
            let len = match &limit.period {
                LimitPeriod::Anchored(limit) => {
                    let rule = limit.rule();
                    let period = rule.containing(t)?;
                    if rule.unit().fixed_ms().is_none() {
                        changes = Some(changes.map_or(period.ends_at, |c| c.min(period.ends_at)));
                    }
                    period.len_ms()
                }
                LimitPeriod::Rolling { every, unit } => LimitPeriod::rolling_len(*every, *unit)?,
            };
            longest = longest.max(len);
        }
        Ok((longest, changes))
    }

    /// The `end` of an executor batch's `server_time_in(at, end)` condition (Paykit spec 8.3,
    /// Paykit wire 5.5): `at + 60 s`, cut at the next boundary of any anchored limit, so the
    /// entry lies in exactly one period of every anchored limit.
    ///
    /// Both bounds are written in the condition, so each is a time the grammar writes, at
    /// most 9999-12-31T23:59:59.999Z (Switch wire 1.6).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an `at`, the executor's own stated time, or an `end` past the
    /// last time the grammar writes; [`Error::Invalid`] for a period of the terms outside
    /// the times the grammar writes.
    pub fn entry_window_end(&self, at: u64) -> Result<u64> {
        let beyond = || {
            Error::Validation(
                "Paykit wire 5.5: an executor batch's window ends past the last time Switch wire 1.6 writes"
                    .into(),
            )
        };
        if at > MAX_TIME_MS {
            return Err(beyond());
        }
        // Saturating is safe: an end past the time grammar is refused just below, and
        // u64::MAX is past it.
        let mut end = at.saturating_add(ENTRY_WINDOW_MS);
        for limit in &self.period_limits {
            if let LimitPeriod::Anchored(anchored) = &limit.period {
                let rule = anchored.rule();
                end = end.min(rule.containing(at)?.ends_at);
            }
        }
        if end > MAX_TIME_MS {
            return Err(beyond());
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

/// A period limit as spelled on the wire, which decides whether two entries are duplicates
/// (Paykit wire 4.15, `specs/allowances.md`): `amount_limit` and `anchor` compare as strings,
/// so `"1"` and `"1.0"`, or two spellings of one instant, are distinct entries;
/// `payment_count_limit` and `every` compare as integers, `kind` and `unit` exactly.
/// Members readers ignore don't count.
#[derive(PartialEq, Eq)]
struct LimitSpelling<'a> {
    amount_limit: Option<&'a str>,
    payment_count_limit: Option<u64>,
    kind: &'a str,
    every: u64,
    unit: &'a str,
    anchor: Option<&'a str>,
}

/// Read a period limit, with its wire spelling.
fn read_period_limit(item: &Value) -> Result<(PeriodLimit, LimitSpelling<'_>)> {
    let limit = Obj::of(item, TERMS, "period_limits")?;
    let period = limit.req_obj("period")?;
    let every = period.req_int("every")?;
    let unit_text = period.req_str("unit")?;
    let unit = Unit::parse(unit_text)?;
    let kind = period.req_str("kind")?;
    let mut anchor = None;
    let period = match kind {
        "anchored" => {
            let text = period.req_str("anchor")?;
            anchor = Some(text);
            LimitPeriod::Anchored(AnchoredLimit::read(
                TermsTime::read(text, "anchor")?,
                every,
                unit,
            )?)
        }
        "rolling" => {
            ensure(every >= 1, "Paykit wire 4.15: every is at least 1")?;
            // Paykit wire 4.15: a rolling unit is a fixed one, minute to week. Its length,
            // every units, isn't computed here: one too long to represent makes the
            // evaluation that needs it ineligible (`specs/allowances.md`), not the terms
            // invalid.
            ensure(
                unit.fixed_ms().is_some(),
                "Paykit wire 4.15: a rolling period's unit is minute, hour, day or week",
            )?;
            LimitPeriod::Rolling { every, unit }
        }
        _ => return Err(bad(TERMS, "period", "kind is anchored or rolling")),
    };
    // Paykit wire 4.15: both limits are present, each possibly null; a missing one isn't
    // read as null.
    for member in ["amount_limit", "payment_count_limit"] {
        if !limit.has(member) {
            return Err(bad(
                TERMS,
                member,
                "missing; a period limit carries both limits",
            ));
        }
    }
    let amount_text = limit.opt_str("amount_limit")?;
    let out = PeriodLimit {
        amount_limit: amount_text.map(Decimal::parse).transpose()?,
        payment_count_limit: limit.opt_int("payment_count_limit")?,
        period,
    };
    ensure(
        out.amount_limit.is_some() || out.payment_count_limit.is_some(),
        "Paykit wire 4.15: a period limit has an amount limit, a count limit or both",
    )?;
    let spelling = LimitSpelling {
        amount_limit: amount_text,
        payment_count_limit: out.payment_count_limit,
        kind,
        every,
        unit: unit_text,
        anchor,
    };
    Ok((out, spelling))
}

fn period_limit_json(limit: &PeriodLimit) -> Value {
    let mut period = Map::new();
    match &limit.period {
        LimitPeriod::Anchored(anchored) => {
            let rule = anchored.rule();
            period.insert(
                "anchor".into(),
                Value::String(anchored.anchor().as_str().into()),
            );
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
    /// Hash of the original signed acceptance in the permanent Allowance record.
    pub acceptance: [u8; 32],
    /// One entry per attempt under the Allowance.
    pub entries: Vec<LedgerEntry>,
    /// `totals.spent`: paid amounts of compacted entries; `None` for `""`, a ledger never
    /// compacted, which reads as zero.
    pub spent: Option<Decimal>,
    /// `totals.count`: payments of compacted entries.
    pub count: u64,
    /// `totals.failed`: compacted `failed` entries.
    pub failed: Vec<CompactedFailed>,
    /// The members the writer doesn't know, kept as read (Paykit wire 5).
    pub unknown: UnknownMembers,
}

static ENTRY_SHAPE: Shape = Shape {
    name: "ledger entry",
    id: &["attempt_id"],
    carried: false,
    members: &[
        ("obligation", Member::Leaf),
        ("attempt_id", Member::Leaf),
        ("value", Member::Leaf),
        ("asset", Member::Leaf),
        ("at", Member::Leaf),
        ("outcome", Member::Leaf),
    ],
};

static FAILED_SHAPE: Shape = Shape {
    name: "totals.failed item",
    id: &["attempt_id"],
    carried: false,
    members: &[
        ("obligation", Member::Leaf),
        ("attempt_id", Member::Leaf),
        ("value", Member::Leaf),
    ],
};

static TOTALS_SHAPE: Shape = Shape {
    name: "totals",
    id: &[],
    carried: false,
    members: &[
        ("spent", Member::Leaf),
        ("count", Member::Leaf),
        ("failed", Member::Array(&FAILED_SHAPE)),
    ],
};

/// A ledger value (Paykit wire 5.5).
static LEDGER_SHAPE: Shape = Shape {
    name: "allowance_ledgers",
    id: &[],
    carried: false,
    members: &[
        ("v", Member::Leaf),
        ("counterparty", Member::Leaf),
        ("allowance_id", Member::Leaf),
        ("acceptance", Member::Leaf),
        ("entries", Member::Array(&ENTRY_SHAPE)),
        ("totals", Member::Object(&TOTALS_SHAPE)),
    ],
};

const LEDGER: &str = "Paykit wire 5.5: allowance_ledgers";

#[derive(Serialize)]
struct LedgerOut<'a> {
    v: u64,
    counterparty: String,
    allowance_id: &'a str,
    acceptance: String,
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
    /// An empty ledger value for the Allowee `counterparty` and Allowance `allowance_id`
    /// (Paykit wire 5.5). Use [`Ledger::initial_value`] for the acceptance batch.
    /// Establishing a spending anchor also requires the verified write and grant
    /// (Paykit spec 8.3.1).
    pub fn empty(counterparty: [u8; 32], allowance_id: AllowanceId, acceptance: [u8; 32]) -> Self {
        Self {
            counterparty,
            allowance_id,
            acceptance,
            entries: Vec::new(),
            spent: None,
            count: 0,
            failed: Vec::new(),
            unknown: UnknownMembers::default(),
        }
    }

    fn same_allowance(&self, other: &Self) -> bool {
        self.counterparty == other.counterparty
            && self.allowance_id == other.allowance_id
            && self.acceptance == other.acceptance
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
        let ledger = Self {
            counterparty: obj.req_pubky("counterparty")?,
            allowance_id: AllowanceId::parse(obj.req_str("allowance_id")?)?,
            acceptance: obj.req_b64::<32>("acceptance")?,
            entries,
            spent,
            count,
            failed,
            unknown: UnknownMembers::read(map, &LEDGER_SHAPE).0,
        };
        ledger.check_attempts_once()?;
        Ok(ledger)
    }

    /// Each `attempt_id` appears once among the entries and `totals.failed` (Paykit wire 5.5
    /// "A valid ledger"), so no attempt is counted twice.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an attempt listed twice.
    fn check_attempts_once(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        let ids = self
            .entries
            .iter()
            .map(|e| e.attempt_id)
            .chain(self.failed.iter().map(|f| f.attempt_id));
        for id in ids {
            if !seen.insert(id) {
                return invalid(
                    "Paykit wire 5.5: an attempt listed twice among the entries and totals.failed",
                );
            }
        }
        Ok(())
    }

    /// The exact initial ledger of the Allower's acceptance batch (Paykit wire 5.9).
    ///
    /// The acceptance is the hash of the recipient's original signed reply.
    pub fn initial_value(
        counterparty: &[u8; 32],
        id: &AllowanceId,
        acceptance: &[u8; 32],
    ) -> Vec<u8> {
        // The three encodings have restricted ASCII alphabets and require no JSON escaping.
        format!(
            r#"{{"v":1,"counterparty":"{}","allowance_id":"{}","acceptance":"{}"}}"#,
            pubky_id(counterparty),
            id.as_str(),
            b64(acceptance),
        )
        .into_bytes()
    }

    /// Write the ledger value: every entry with its `outcome` (`null` while unknown);
    /// `totals` with `spent` `""` for a ledger never compacted, else in the normalized
    /// spelling of a computed amount (Paykit wire 1.2), and `failed` left out when empty;
    /// and the members it doesn't know as it read them (Paykit wire 5). The value is read
    /// back by [`Ledger::from_json`] before it is handed out, so the writer never writes a
    /// ledger its readers refuse. The output always includes `entries` and `totals`,
    /// even for an empty ledger.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a ledger readers refuse, such as an attempt listed twice
    /// (Paykit wire 5.5), or one that doesn't read back as itself; [`Error::Invalid`] if the
    /// value can't be written.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        let json = self.json_unchecked()?;
        let back = Self::from_json(&json).map_err(Error::into_validation)?;
        // The value, and the bytes: the wire fixes the spelling of a computed amount, which a
        // comparison of values alone, by scaled amount, wouldn't see. The unknown members are
        // compared as written: those of an object the ledger no longer holds, such as a
        // compacted entry, are left out with it.
        let known = |l: &Ledger| Ledger {
            unknown: UnknownMembers::default(),
            ..l.clone()
        };
        if known(&back) != known(self) || back.json_unchecked()? != json {
            return Err(Error::Validation(format!(
                "{LEDGER}: the ledger doesn't write as itself"
            )));
        }
        Ok(json)
    }

    fn json_unchecked(&self) -> Result<Vec<u8>> {
        let out = LedgerOut {
            v: 1,
            counterparty: pubky_id(&self.counterparty),
            allowance_id: self.allowance_id.as_str(),
            acceptance: b64(&self.acceptance),
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
                    .map_or_else(String::new, Decimal::normalized),
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
        let json = serde_json::to_vec(&out)
            .map_err(|_| Error::Invalid(format!("{LEDGER}: can't be written")))?;
        self.unknown.write_into(json, &LEDGER_SHAPE, &[])
    }

    /// "A valid ledger" against the terms (Paykit wire 5.5): the terms are valid (Paykit wire
    /// 4.15), every entry's asset is the terms' asset and its `at` a time the grammar writes,
    /// and each attempt appears once among the entries and `totals.failed`.
    /// Usage, compaction and the check of a newer version refuse a ledger that isn't valid,
    /// a typed one included, so they never count an attempt twice.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for terms that aren't valid; [`Error::Invalid`] for an entry of
    /// another asset or time, or an attempt listed twice.
    pub fn check_valid(&self, terms: &AllowanceTerms) -> Result<()> {
        // Terms built in code are held to the reader's rules too, such as `every` at least 1
        // (Paykit wire 4.15): a limit no reader takes never weakens an evaluation.
        terms.to_json()?;
        ensure(
            self.entries.iter().all(|e| e.asset == terms.asset),
            "Paykit wire 5.5: a ledger entry in another asset than the terms'",
        )?;
        ensure(
            self.entries.iter().all(|e| e.at <= MAX_TIME_MS),
            "Paykit wire 5.5: a ledger entry's at past the last time Switch wire 1.6 writes",
        )?;
        self.check_attempts_once()
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
    /// Counted as spent, including a failed attempt without never-dispatched evidence.
    Spent,
    /// Freed by durable evidence that dispatch never started and cannot start.
    Released,
}

/// Evidence used to count an attempt (Paykit spec 8.3, Paykit wire 5.5).
/// Claim state alone never proves a failed payment was not dispatched. Callers must
/// validate and bind this evidence to the entry's attempt, obligation, Allowance,
/// amount, asset and owner before using it to count usage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttemptEvidence {
    /// The attempt's current claim state, absent when the claim or attempt is gone.
    pub claim: Option<ClaimState>,
    /// The payer's validated proof or durable journal shows the attempt was paid.
    pub paid: bool,
    /// The executor's durable journal proves the attempt ended before the payment
    /// method's call and can never dispatch. A restored journal, user confirmation or
    /// reported failure is insufficient. This evidence must bind to this attempt.
    pub never_dispatched: bool,
}

impl From<ClaimState> for AttemptEvidence {
    fn from(claim: ClaimState) -> Self {
        Self {
            claim: Some(claim),
            ..Self::default()
        }
    }
}

/// How an entry counts (Paykit spec 8.3, Paykit wire 5.5). A paid outcome, claim or
/// proof spends. Every failed attempt spends unless durable never-dispatched evidence
/// releases it. Without an outcome, open and unknown reserve, and a missing claim spends.
pub fn counts(entry: &LedgerEntry, evidence: AttemptEvidence) -> Counting {
    counting(entry.outcome, evidence)
}

fn counting(outcome: Option<EntryOutcome>, evidence: AttemptEvidence) -> Counting {
    if outcome == Some(EntryOutcome::Done)
        || evidence.claim == Some(ClaimState::Done)
        || evidence.paid
    {
        return Counting::Spent;
    }
    if outcome == Some(EntryOutcome::Failed)
        || matches!(evidence.claim, Some(ClaimState::Failed { .. }))
    {
        return if evidence.never_dispatched {
            Counting::Released
        } else {
            Counting::Spent
        };
    }
    match evidence.claim {
        Some(ClaimState::Open | ClaimState::Unknown) => Counting::Reserved,
        _ => Counting::Spent,
    }
}

/// One period limit's usage: the amount and count of the entries it counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeriodUsage {
    amount: Decimal,
    count: u64,
}

impl PeriodUsage {
    /// The amount counted.
    pub fn amount(&self) -> &Decimal {
        &self.amount
    }

    /// The payments counted.
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// Usage of an Allowance at one time (Paykit spec 8.3, Paykit wire 5.5), as [`usage`]
/// computes it for one set of terms. Only [`usage`] builds one, and it holds the period
/// limits it was computed for, so [`check_limits`] never takes usage of other terms, nor one
/// that leaves a limit out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    lifetime: Decimal,
    periods: Vec<PeriodUsage>,
    reserved: u64,
    reserved_amount: Decimal,
    spent: u64,
    released: u64,
    limits: Vec<PeriodLimit>,
}

impl Usage {
    /// The lifetime amount: compacted spent totals and counted entries, including
    /// compacted failed attempts. Failures stay spent unless bound durable
    /// never-dispatched evidence releases them; `done` or paid evidence always spends.
    pub fn lifetime(&self) -> &Decimal {
        &self.lifetime
    }

    /// Each period limit's usage, in the order of `period_limits`.
    pub fn periods(&self) -> &[PeriodUsage] {
        &self.periods
    }

    /// Entries reserved.
    pub fn reserved(&self) -> u64 {
        self.reserved
    }

    /// The amount of the entries reserved, counted in [`Usage::lifetime`]: the room their
    /// outcomes may give back, as a never-dispatched failure releases, with no one acting.
    pub fn reserved_amount(&self) -> &Decimal {
        &self.reserved_amount
    }

    /// Entries spent.
    pub fn spent(&self) -> u64 {
        self.spent
    }

    /// Entries released.
    pub fn released(&self) -> u64 {
        self.released
    }
}

fn overflow() -> Error {
    Error::Invalid("Paykit wire 1.2: an Allowance sum overflows; usage fails closed".into())
}

fn add(a: u128, b: u128) -> Result<u128> {
    a.checked_add(b).ok_or_else(overflow)
}

fn in_limit(period: &LimitPeriod, entry_at: u64, t: u64) -> Result<bool> {
    Ok(match period {
        LimitPeriod::Anchored(anchored) => anchored.rule().containing(t)?.contains(entry_at),
        LimitPeriod::Rolling { every, unit } => {
            let len = LimitPeriod::rolling_len(*every, *unit)?;
            // An entry counts while `t < at + 60 s + L`, with no lower bound. A bound that
            // can't be represented makes the evaluation ineligible (`specs/allowances.md`).
            let until = entry_at
                .checked_add(ENTRY_WINDOW_MS)
                .and_then(|x| x.checked_add(len))
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "{TERMS}: a rolling window's end can't be represented; usage fails closed"
                    ))
                })?;
            t < until
        }
    })
}

/// Usage at time `t` (Paykit spec 8.3, Paykit wire 5.5): every entry counted by
/// [`counts`] with its attempt's `evidence`, for every period limit and the lifetime
/// amount. Missing evidence never releases capacity. A `totals.failed` item counts by
/// the same failed-attempt rule, in the lifetime amount only.
///
/// # Errors
///
/// [`Error::Invalid`] for a ledger that isn't valid against the terms, or a sum that
/// overflows: usage never reads a ledger as smaller.
pub fn usage(
    terms: &AllowanceTerms,
    ledger: &Ledger,
    evidence: &HashMap<AttemptId, AttemptEvidence>,
    t: u64,
) -> Result<Usage> {
    ledger.check_valid(terms)?;
    // Every limit's period at t, whatever the entries: a duration or boundary that can't be
    // represented makes the evaluation ineligible (`specs/allowances.md`), never a limit
    // left out.
    for limit in &terms.period_limits {
        match &limit.period {
            LimitPeriod::Anchored(anchored) => {
                anchored.rule().containing(t)?;
            }
            LimitPeriod::Rolling { .. } => {
                // The candidate, at t, counts by the same bound as an entry.
                in_limit(&limit.period, t, t)?;
            }
        }
    }
    let mut lifetime = ledger.spent_scaled();
    let mut periods = vec![(0u128, 0u64); terms.period_limits.len()];
    let (mut reserved, mut spent, mut released) = (0u64, 0u64, 0u64);
    let mut reserved_amount = 0u128;
    for item in &ledger.failed {
        if counting(
            Some(EntryOutcome::Failed),
            evidence.get(&item.attempt_id).copied().unwrap_or_default(),
        ) != Counting::Released
        {
            lifetime = add(lifetime, item.value.scaled())?;
        }
    }
    for entry in &ledger.entries {
        match counts(
            entry,
            evidence.get(&entry.attempt_id).copied().unwrap_or_default(),
        ) {
            // Counts of the ledger's entries, informational: a ledger holds fewer entries than
            // a u64 counts, so these never saturate; the limits use the checked sums below.
            Counting::Reserved => {
                reserved = reserved.saturating_add(1);
                reserved_amount = add(reserved_amount, entry.value.scaled())?;
            }
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
        reserved_amount: Decimal::from_scaled(reserved_amount),
        spent,
        released,
        limits: terms.period_limits.clone(),
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
    /// Usage that isn't of these terms' period limits, one for each in order:
    /// `inconsistent_usage`. Every candidate is refused against it.
    InconsistentUsage,
}

impl LimitKind {
    /// The limit's name, as the vectors' `refused_by` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PerPayment => "per_payment",
            Self::PeriodAmount => "period_amount",
            Self::PeriodCount => "period_count",
            Self::Lifetime => "lifetime",
            Self::InconsistentUsage => "inconsistent_usage",
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
    // Every limit, each with its own usage: usage of other terms, or one that leaves a limit
    // out or adds one, refuses rather than skip a limit.
    if usage.limits != terms.period_limits || usage.periods.len() != terms.period_limits.len() {
        return Err(LimitKind::InconsistentUsage);
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
///
/// # Errors
///
/// [`Error::Invalid`] for a bound that can't be represented, which makes the compaction or
/// the check of a newer version ineligible (`specs/allowances.md`), never an entry read as
/// too young or old enough.
fn old_enough(at: u64, longest: u64, now: u64) -> Result<bool> {
    at.checked_add(longest)
        .and_then(|x| x.checked_add(ENTRY_WINDOW_MS))
        .map(|x| x < now)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "{LEDGER}: an entry's compaction bound can't be represented; it fails closed"
            ))
        })
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
        let outcome = match entry.outcome {
            Some(outcome) if old_enough(entry.at, longest, now)? => Some(outcome),
            _ => None,
        };
        match outcome {
            Some(outcome) => match outcome {
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
        acceptance: ledger.acceptance,
        entries: kept,
        spent: Some(Decimal::from_scaled(spent)),
        count,
        failed,
        unknown: ledger.unknown.clone(),
    }))
}

/// Copy verified outcomes into the executor's ledger (Paykit spec 8.3, Paykit wire 5.5).
/// The caller binds each claim or payer proof to the reservation. For this write, `paid`
/// means the payer record holds a verified proof for this attempt and Allowance.
/// An owner-written failure can fill an unset outcome. A paid claim or proof takes
/// precedence and moves a compacted failed attempt into the paid totals exactly once.
/// Returns `None` if the evidence makes no change.
///
/// # Errors
///
/// Returns an error for an invalid ledger or terms, or if a paid total would overflow.
pub fn record_outcomes(
    terms: &AllowanceTerms,
    ledger: &Ledger,
    evidence: &HashMap<AttemptId, AttemptEvidence>,
) -> Result<Option<Ledger>> {
    ledger.check_valid(terms)?;
    let paid = |fact: &AttemptEvidence| fact.paid || fact.claim == Some(ClaimState::Done);
    let mut next = None;
    for (index, entry) in ledger.entries.iter().enumerate() {
        let Some(fact) = evidence.get(&entry.attempt_id) else {
            continue;
        };
        let outcome = if entry.outcome != Some(EntryOutcome::Done) && paid(fact) {
            EntryOutcome::Done
        } else if entry.outcome.is_none()
            && fact.claim == Some(ClaimState::Failed { by_owner: true })
        {
            EntryOutcome::Failed
        } else {
            continue;
        };
        next.get_or_insert_with(|| ledger.clone()).entries[index].outcome = Some(outcome);
    }
    let was_paid = |item: &CompactedFailed| evidence.get(&item.attempt_id).is_some_and(paid);
    let mut spent = ledger.spent_scaled();
    let mut count = ledger.count;
    for item in ledger.failed.iter().filter(|item| was_paid(item)) {
        spent = add(spent, item.value.scaled())?;
        count = count.checked_add(1).ok_or_else(overflow)?;
    }
    if count != ledger.count {
        let new = next.get_or_insert_with(|| ledger.clone());
        new.spent = Some(Decimal::from_scaled(spent));
        new.count = count;
        new.failed.retain(|item| !was_paid(item));
    }
    Ok(next)
}

/// "From one version to the next" (Paykit spec 8.3, Paykit wire 5.5): a reader that holds
/// `held`, written at service time `held_time`, accepts `newer`, written at service time
/// `newer_time`, only if it counts no less.
///
/// Every held entry is still there with the same obligation, attempt, value, asset and time,
/// its outcome unchanged or gone from none to `done` or `failed`, or from `failed` to
/// `done`; or it was compacted, old enough at some service time from `held_time` to
/// `newer_time` by the longest period at that time (writes the reader didn't see may have
/// compacted it then): a `done` one into the totals, and one without an outcome or `failed`
/// into `totals.failed` with the same obligation and value, or into the totals. Every held
/// `totals.failed` item is still there unchanged or left into the totals, and `spent` and
/// `count` grew by at least what left into them. New entries are allowed.
///
/// So a monthly limit's entry that a write at the end of February compacted is accepted in
/// a version written in March, though March's longer period would keep it at that time.
/// A `held_time` later than `newer_time` (on the service's clock a newer version is never
/// written before the one it follows; a time the reader took itself may be later) leaves no
/// time between the two: the entries are judged at `newer_time` alone.
///
/// # Errors
///
/// [`Error::Invalid`] naming the rule broken: the reader ignores and flags that version; also
/// for a longest period at a time judged that can't be represented, which makes the check
/// ineligible (`specs/allowances.md`).
pub fn check_successor(
    terms: &AllowanceTerms,
    held: &Ledger,
    held_time: u64,
    newer: &Ledger,
    newer_time: u64,
) -> Result<()> {
    held.check_valid(terms)?;
    newer.check_valid(terms)?;
    let refuse = |why: &str| invalid(format!("Paykit wire 5.5: a newer ledger version {why}"));
    if !newer.same_allowance(held) {
        return refuse("is another Allowance's ledger");
    }
    // Terms whose longest period can't be represented at the newer write's time make the
    // check ineligible, whatever the two versions hold (`specs/allowances.md`).
    terms.longest_period(newer_time)?;
    let from = held_time.min(newer_time);
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
        if !compactable_between(terms, entry.at, from, newer_time)? {
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
        // Into the totals once: the attempt is then counted in spent and count, so it can't
        // also come back as an entry.
        if newer
            .entries
            .iter()
            .any(|e| e.attempt_id == item.attempt_id)
        {
            return refuse("moves a totals.failed item into the totals and keeps it as an entry");
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

/// Whether an entry at `at` was old enough to compact ([`compact`]) at some service time from
/// `from` to `to`, both included, by the longest period at that time (Paykit spec 8.3, Paykit
/// wire 5.5 "From one version to the next").
///
/// Between two ends of periods of the anchored limits in months or years the longest period
/// is the same, and an entry old enough at a time is old enough at every later time of that
/// span, so the span's last millisecond decides it. No time up to `at + 60 s` is late enough,
/// whatever the period, and every time past `at + 60 s` plus the terms' longest possible
/// period is: the spans walked are those between, from `from`, up to the first that decides
/// it, so their number depends on the terms alone, never on how far apart `from` and `to`
/// are.
///
/// # Errors
///
/// As [`AllowanceTerms::longest_period`] at a time judged, and as [`old_enough`].
fn compactable_between(terms: &AllowanceTerms, at: u64, from: u64, to: u64) -> Result<bool> {
    // Saturating is safe: a valid ledger's entry is at most MAX_TIME_MS, far from u64::MAX,
    // and a saturated start would only leave no time to judge.
    let mut t = from.max(at.saturating_add(ENTRY_WINDOW_MS).saturating_add(1));
    while t <= to {
        let (longest, changes) = terms.longest_period_span(t)?;
        // `changes` is after `t`, so the span is not empty.
        let last = changes.map_or(to, |c| to.min(c - 1));
        if old_enough(at, longest, last)? {
            return Ok(true);
        }
        match last.checked_add(1) {
            Some(next) => t = next,
            None => break,
        }
    }
    Ok(false)
}

/// Whether `ledger` lists `attempt` among its entries or in `totals.failed`.
fn lists(ledger: &Ledger, attempt: &AttemptId) -> bool {
    ledger.entries.iter().any(|e| e.attempt_id == *attempt)
        || ledger.failed.iter().any(|f| f.attempt_id == *attempt)
}

/// Whether `entry` can be one of an Allowance under `terms`: in the terms' asset, at a time
/// the grammar writes ("A valid ledger", Paykit wire 5.5).
fn of_the_terms(terms: &AllowanceTerms, entry: &LedgerEntry) -> bool {
    entry.asset == terms.asset && entry.at <= MAX_TIME_MS
}

/// The larger of `a` and `b`.
fn larger(a: &Decimal, b: &Decimal) -> Decimal {
    if b > a {
        b.clone()
    } else {
        a.clone()
    }
}

/// The ledger a repair writes (Paykit spec 8.3, "After a confirmation"; Paykit wire 5.5): a
/// merge that never counts less, from:
///
/// - `kept`, the complete accepted ledger whose authority the caller has established.
///   An empty value is usable only when it is that qualified checkpoint; absence is not
///   zero usage. Its entries and `totals.failed` items stay as they are, so the result is
///   its successor ([`check_successor`]); only a `done` another input shows is taken, and a
///   `totals.failed` item shown paid moves into `spent` and `count` once;
/// - `shown`, the ledger the service shows now, which the executor didn't accept: its higher
///   `spent` and `count`, and the attempts `kept` doesn't list;
/// - `journal`, the attempts the executor's journal holds under the Allowance, and `claims`,
///   the attempts the claims of the counterparty's obligations show, with a `done` where a
///   payer or period record's proof shows the payment made under the Allowance: the payer
///   identity's own evidence.
///
/// For an attempt `kept` doesn't list, the entry takes what counts it most: the largest
/// value any input shows, the latest `at`, so it counts in every limit an earlier one would
/// from then on, an `at` after `now`, the repair's service time, counting as `now`, since no
/// attempt is made after it; the obligation of the journal or the claim; and `done` where an
/// input shows it. A `failed` outcome shown is never taken, and a `totals.failed` item shown
/// stays one only for an attempt neither the journal nor a claim shows: otherwise it is an
/// entry, which counts by its claim in every limit. A shown copy, such as a removed app's
/// write, so never counts an attempt the payer's own evidence shows at less than that
/// evidence does.
///
/// An attempt `kept` lists counts as `kept` has it: the executor accepted that entry, and
/// every reader holding that version checks the next one against it. Own evidence that
/// shows such an attempt at a larger value refuses the merge: it can't be counted as both.
/// So does own evidence dated at or after the end of the kept entry's window
/// ([`AllowanceTerms::entry_window_end`]): the executor's batch lands before that end, so
/// such evidence isn't of that batch. Evidence dated earlier, within the window, is the
/// service time the batch landed at, and the entry already counts the attempt from then on:
/// it counts while `t < at + 60 s + L` in a rolling limit, past the attempt's own `t < e + L`
/// for any `e` before `at + 60 s`, and the window is cut at the next boundary of any anchored
/// limit, so `e` lies in the entry's period. A `failed` outcome `kept` holds stays, as the
/// attempt's owner or the user wrote it, and a `totals.failed` item counts in the lifetime
/// amount only, whatever its time.
///
/// The merge may count an attempt twice, never less: an entry `shown` lists that `kept`
/// compacted into `spent` and `count` is added again, as nothing in the inputs tells it
/// apart from one `kept` never held.
///
/// An entry of another asset than the terms', or at a time the grammar doesn't write, is
/// left out: no executor makes an attempt in another asset under the Allowance (Paykit spec
/// 8.3, "Executor"). Usage over the result at `now` and later, by every limit, is no less
/// than over `kept` and over what `shown` holds of attempts `kept` doesn't list, and the
/// result counts no less than any evidence, per attempt, at the service time it was made.
///
/// The caller must establish complete usage authority before this calculation: retain
/// the full accepted value and authenticated anchor, match the exact current value in
/// the same uninterrupted account history, and resolve every known prepared write
/// (Paykit spec 8.3, "Complete usage authority" and "Keeping that authority"). This pure
/// merge neither obtains that evidence nor authorizes a write or payment. A shown value,
/// prior version number, confirmation or empty inventory cannot establish the checkpoint.
///
/// Every `journal` or `claims` entry must already carry a qualified exact transfer amount
/// bound to its attempt, obligation, counterparty, Allowance and asset, including rounding
/// and excluding fees (Paykit spec 8.3, "Amounts recovered by a repair"). A raw claim,
/// amount-free proof, nominal request amount or unaccepted shown value cannot supply a
/// missing amount. If either authority or amount evidence is missing, the caller must
/// refuse the repair rather than call this function with incomplete inputs.
/// Unknown members of retained objects are kept from both ledgers. Different values for
/// the same unknown member refuse the merge (Paykit wire 5).
///
/// # Errors
///
/// [`Error::Invalid`] for a `kept` that isn't valid against the terms, a `shown` of another
/// Allowance, own evidence of a larger value than `kept` holds for one of its attempts or
/// dated after that entry's window, conflicting retained unknown members, or a sum that
/// overflows; [`Error::Validation`] for a
/// kept entry's window that ends past the last time the grammar writes.
pub fn merge_for_repair(
    terms: &AllowanceTerms,
    kept: &Ledger,
    shown: Option<&Ledger>,
    journal: &[LedgerEntry],
    claims: &[LedgerEntry],
    now: u64,
) -> Result<Ledger> {
    kept.check_valid(terms)?;
    if let Some(shown) = shown {
        if !shown.same_allowance(kept) {
            return invalid("Paykit wire 5.5: the ledger shown is another Allowance's");
        }
    }
    // The payer identity's own evidence, one entry per attempt: the journal's obligation,
    // else the claim's, the largest value, the latest time and a `done` either shows.
    let mut own: Vec<LedgerEntry> = Vec::new();
    for entry in journal
        .iter()
        .chain(claims)
        .filter(|e| of_the_terms(terms, e))
    {
        if let Some(accepted) = kept
            .entries
            .iter()
            .find(|accepted| accepted.attempt_id == entry.attempt_id)
        {
            // An accepted entry keeps its executor window. Capping an evidence time at
            // the repair time must not make an incompatible time fit that window.
            if entry.at >= terms.entry_window_end(accepted.at)? {
                return invalid(
                    "Paykit spec 8.3: an attempt's own evidence shows it made after the window of the version kept",
                );
            }
        }
        let at = entry.at.min(now);
        let done = entry.outcome == Some(EntryOutcome::Done);
        match own.iter_mut().find(|o| o.attempt_id == entry.attempt_id) {
            Some(held) => {
                held.value = larger(&held.value, &entry.value);
                held.at = held.at.max(at);
                if done {
                    held.outcome = Some(EntryOutcome::Done);
                }
            }
            None => own.push(LedgerEntry {
                at,
                outcome: done.then_some(EntryOutcome::Done),
                ..entry.clone()
            }),
        }
    }
    let own_of = |attempt: &AttemptId| own.iter().find(|o| o.attempt_id == *attempt);
    let shown_done = |attempt: &AttemptId| {
        shown.is_some_and(|l| {
            l.entries.iter().any(|e| {
                e.attempt_id == *attempt
                    && e.outcome == Some(EntryOutcome::Done)
                    && of_the_terms(terms, e)
            })
        })
    };
    let more_than_kept = || {
        invalid(
            "Paykit spec 8.3: an attempt's own evidence shows a larger value than the version kept",
        )
    };
    let mut next = kept.clone();
    let mut spent = next.spent_scaled();
    let mut count = next.count;
    if let Some(shown) = shown {
        spent = spent.max(shown.spent_scaled());
        count = count.max(shown.count);
    }
    // The version kept, as it was: a `done` any input shows is taken. Own evidence dated
    // within the kept entry's window, up to the latest service time its executor batch
    // could land at, is that batch: the entry already counts the attempt from then on.
    // Evidence dated later can't be the same batch, and the entry can't be moved.
    for entry in &mut next.entries {
        let evidence = own_of(&entry.attempt_id);
        if evidence.is_some_and(|o| o.value > entry.value) {
            return more_than_kept();
        }
        if shown_done(&entry.attempt_id)
            || evidence.is_some_and(|o| o.outcome == Some(EntryOutcome::Done))
        {
            entry.outcome = Some(EntryOutcome::Done);
        }
    }
    // A compacted failed attempt shown paid: into the totals, once.
    let mut failed = Vec::with_capacity(next.failed.len());
    for item in std::mem::take(&mut next.failed) {
        let evidence = own_of(&item.attempt_id);
        if evidence.is_some_and(|o| o.value > item.value) {
            return more_than_kept();
        }
        if shown_done(&item.attempt_id)
            || evidence.is_some_and(|o| o.outcome == Some(EntryOutcome::Done))
        {
            spent = add(spent, item.value.scaled())?;
            count = count.checked_add(1).ok_or_else(overflow)?;
        } else {
            failed.push(item);
        }
    }
    next.failed = failed;
    // The attempts the version kept doesn't list, each counted the most any input shows.
    let mut added: Vec<LedgerEntry> = Vec::new();
    let mut added_failed: Vec<CompactedFailed> = Vec::new();
    if let Some(shown) = shown {
        for entry in shown.entries.iter().filter(|e| of_the_terms(terms, e)) {
            if lists(kept, &entry.attempt_id) {
                continue;
            }
            let done = entry.outcome == Some(EntryOutcome::Done);
            let at = entry.at.min(now);
            added.push(match own_of(&entry.attempt_id) {
                Some(evidence) => LedgerEntry {
                    value: larger(&evidence.value, &entry.value),
                    at: evidence.at.max(at),
                    outcome: (done || evidence.outcome.is_some()).then_some(EntryOutcome::Done),
                    ..evidence.clone()
                },
                None => LedgerEntry {
                    at,
                    outcome: done.then_some(EntryOutcome::Done),
                    ..entry.clone()
                },
            });
        }
        for item in &shown.failed {
            if lists(kept, &item.attempt_id)
                || added.iter().any(|e| e.attempt_id == item.attempt_id)
            {
                continue;
            }
            match own_of(&item.attempt_id) {
                // Open, unknown or paid by the payer's own evidence: an entry, never a
                // compacted failed one.
                Some(evidence) => added.push(LedgerEntry {
                    value: larger(&evidence.value, &item.value),
                    ..evidence.clone()
                }),
                None => added_failed.push(item.clone()),
            }
        }
    }
    for evidence in &own {
        if !lists(kept, &evidence.attempt_id)
            && !added.iter().any(|e| e.attempt_id == evidence.attempt_id)
        {
            added.push(evidence.clone());
        }
    }
    next.entries.extend(added);
    next.failed.extend(added_failed);
    if spent != next.spent_scaled() {
        next.spent = Some(Decimal::from_scaled(spent));
    }
    next.count = count;
    next.check_valid(terms)?;
    if let Some(shown) = shown {
        // An ignored entry is not retained, even when another input has its attempt ID.
        let mut retained = shown.clone();
        retained.entries.retain(|entry| of_the_terms(terms, entry));
        let retained = Ledger::from_json(&retained.json_unchecked()?)?;
        let known = Ledger {
            unknown: UnknownMembers::default(),
            ..next.clone()
        }
        .json_unchecked()?;
        next.unknown = kept
            .unknown
            .merge_identified(&retained.unknown, &known, &LEDGER_SHAPE)?;
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_switch_core::time::parse_rfc3339;
    use serde_json::json;

    fn usage(
        terms: &AllowanceTerms,
        ledger: &Ledger,
        claims: &HashMap<AttemptId, ClaimState>,
        at: u64,
    ) -> Result<Usage> {
        let evidence = claims
            .iter()
            .map(|(id, state)| (*id, (*state).into()))
            .collect();
        super::usage(terms, ledger, &evidence, at)
    }

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
                [9; 32],
            )
        }
    }

    /// Terms with the period limits `limits`.
    fn with_limits(limits: Value) -> Value {
        let mut v = terms();
        v["period_limits"] = limits;
        v
    }

    #[test]
    fn test_terms_period_limit_amounts_differently_spelled_are_distinct() {
        let limit = |amount: &str| {
            json!({ "amount_limit": amount, "payment_count_limit": null,
                "period": { "kind": "rolling", "every": 1, "unit": "day" } })
        };
        let terms =
            AllowanceTerms::from_json(&with_limits(json!([limit("1"), limit("1.0")]))).unwrap();
        assert_eq!(terms.period_limits.len(), 2);
        let error = AllowanceTerms::from_json(&with_limits(json!([limit("1.0"), limit("1.0")])))
            .unwrap_err();
        assert!(error.to_string().contains("an entry twice"), "{error}");
    }

    #[test]
    fn test_terms_period_limit_anchors_differently_spelled_are_distinct() {
        let limit = |anchor: &str| {
            json!({ "amount_limit": "1", "payment_count_limit": 2,
                "period": { "anchor": anchor, "every": 1, "kind": "anchored", "unit": "month" } })
        };
        let terms = AllowanceTerms::from_json(&with_limits(json!([
            limit("2026-01-31T00:00:00Z"),
            limit("2026-01-31T00:00:00.000Z")
        ])))
        .unwrap();
        assert_eq!(terms.period_limits.len(), 2);
        let error = AllowanceTerms::from_json(&with_limits(json!([
            limit("2026-01-31T00:00:00Z"),
            limit("2026-01-31T00:00:00Z")
        ])))
        .unwrap_err();
        assert!(error.to_string().contains("an entry twice"), "{error}");
        // A member readers ignore doesn't tell two entries apart.
        let mut other = limit("2026-01-31T00:00:00Z");
        other["note"] = json!("x");
        assert!(AllowanceTerms::from_json(&with_limits(json!([
            limit("2026-01-31T00:00:00Z"),
            other
        ])))
        .is_err());
    }

    #[test]
    fn test_terms_write_back_in_the_spelling_read() {
        let limit = |amount: &str, anchor: &str| {
            json!({ "amount_limit": amount, "payment_count_limit": null,
                "period": { "anchor": anchor, "every": 1, "kind": "anchored", "unit": "month" } })
        };
        let mut received = with_limits(json!([
            limit("1", "2026-01-31T00:00:00Z"),
            limit("1.0", "2026-01-31T00:00:00Z"),
            limit("1", "2026-01-31T00:00:00.000Z"),
            limit("1", "2026-01-31T00:00:00.0000Z"),
        ]));
        received["active_from"] = json!("2026-01-01T00:00:00Z");
        received["expires_at"] = json!("2027-01-01T00:00:00.5Z");
        received["lifetime_amount_limit"] = json!("0.100");
        // Read, write, read again: the same entries and the same bytes.
        let first = AllowanceTerms::from_json(&received).unwrap();
        let written = first.to_json().unwrap();
        assert_eq!(
            serde_json::to_vec(&written).unwrap(),
            serde_json::to_vec(&received).unwrap()
        );
        let second = AllowanceTerms::from_json(&written).unwrap();
        assert_eq!(second, first);
        assert_eq!(second.period_limits.len(), 4);
        assert_eq!(
            serde_json::to_vec(&second.to_json().unwrap()).unwrap(),
            serde_json::to_vec(&written).unwrap()
        );
        // So does a proposal carrying them, the one message whose body holds terms.
        let body = json!({ "allowance_id": "d0efffb7-191b-4709-b6c7-e343af5500b4",
            "proposer_role": "allowee", "allower_generation": 1, "terms": received });
        let proposal =
            crate::bodies::AllowanceProposal::from_body(body.as_object().unwrap()).unwrap();
        assert_eq!(Value::Object(proposal.to_body().unwrap()), body);
    }

    #[test]
    fn test_terms_built_locally_take_the_canonical_spelling() {
        let anchor = TermsTime::new(t("2026-01-31T00:00:00Z"));
        assert_eq!(anchor.as_str(), "2026-01-31T00:00:00.000Z");
        let limit = PeriodLimit {
            amount_limit: Some(Decimal::parse("1").unwrap()),
            payment_count_limit: None,
            period: LimitPeriod::Anchored(AnchoredLimit::new(anchor, 1, Unit::Month).unwrap()),
        };
        let terms = AllowanceTerms {
            asset: Asset::parse("btc").unwrap(),
            per_payment_amount: None,
            period_limits: vec![limit],
            lifetime_amount_limit: None,
            active_from: Some(TermsTime::new(t("2026-01-01T00:00:00Z"))),
            expires_at: None,
            allowed_payment_endpoint_identifiers: None,
        };
        let written = terms.to_json().unwrap();
        assert_eq!(written["active_from"], "2026-01-01T00:00:00.000Z");
        assert_eq!(
            written["period_limits"][0]["period"]["anchor"],
            "2026-01-31T00:00:00.000Z"
        );
        assert_eq!(AllowanceTerms::from_json(&written).unwrap(), terms);
    }

    #[test]
    fn test_terms_period_limit_needs_both_limits_present() {
        let full = json!({ "amount_limit": "1", "payment_count_limit": null,
            "period": { "every": 1, "kind": "rolling", "unit": "day" } });
        AllowanceTerms::from_json(&with_limits(json!([full.clone()]))).unwrap();
        for member in ["amount_limit", "payment_count_limit"] {
            let mut missing = full.clone();
            missing.as_object_mut().unwrap().remove(member);
            if member == "amount_limit" {
                missing["payment_count_limit"] = json!(2);
            }
            let error = AllowanceTerms::from_json(&with_limits(json!([missing])))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(member) && error.contains("missing"),
                "{member}: {error}"
            );
        }
    }

    #[test]
    fn test_terms_round_trip() {
        let parsed = AllowanceTerms::from_json(&terms()).unwrap();
        assert_eq!(parsed.to_json().unwrap(), terms());
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
            (Some(ClaimState::Failed { by_owner: true }), Counting::Spent),
            (
                Some(ClaimState::Failed { by_owner: false }),
                Counting::Spent,
            ),
            (None, Counting::Spent),
        ] {
            assert_eq!(
                counts(
                    &open,
                    AttemptEvidence {
                        claim,
                        ..AttemptEvidence::default()
                    }
                ),
                want,
                "{claim:?}"
            );
        }
        let failed = LedgerEntry {
            outcome: Some(EntryOutcome::Failed),
            ..open
        };
        assert_eq!(counts(&failed, ClaimState::Done.into()), Counting::Spent);
        assert_eq!(counts(&failed, AttemptEvidence::default()), Counting::Spent);
    }

    #[test]
    fn test_allowance_failed_attempts_spend_without_dispatch_evidence() {
        let mut attempt = entry(1, "0.01", "2026-02-01T00:00:00.000Z", None);
        for claim in [
            None,
            Some(ClaimState::Open),
            Some(ClaimState::Unknown),
            Some(ClaimState::Failed { by_owner: true }),
            Some(ClaimState::Failed { by_owner: false }),
        ] {
            for outcome in [None, Some(EntryOutcome::Failed)] {
                attempt.outcome = outcome;
                let failed = outcome.is_some() || matches!(claim, Some(ClaimState::Failed { .. }));
                let expected =
                    if !failed && matches!(claim, Some(ClaimState::Open | ClaimState::Unknown)) {
                        Counting::Reserved
                    } else {
                        Counting::Spent
                    };
                let evidence = AttemptEvidence {
                    claim,
                    ..AttemptEvidence::default()
                };
                assert_eq!(counts(&attempt, evidence), expected);
                let never = AttemptEvidence {
                    never_dispatched: true,
                    ..evidence
                };
                assert_eq!(
                    counts(&attempt, never),
                    if failed { Counting::Released } else { expected }
                );
                assert_eq!(
                    counts(
                        &attempt,
                        AttemptEvidence {
                            paid: true,
                            ..never
                        }
                    ),
                    Counting::Spent
                );
                assert_eq!(
                    counts(
                        &attempt,
                        AttemptEvidence {
                            claim: Some(ClaimState::Done),
                            ..never
                        }
                    ),
                    Counting::Spent
                );
            }
        }
        attempt.outcome = Some(EntryOutcome::Done);
        assert_eq!(
            counts(
                &attempt,
                AttemptEvidence {
                    never_dispatched: true,
                    ..AttemptEvidence::default()
                }
            ),
            Counting::Spent
        );
    }

    #[test]
    fn test_allowance_compacted_failure_needs_the_same_dispatch_evidence() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let mut l = ledger(Vec::new());
        let attempt = AttemptId::from_bytes([8; 16]);
        l.failed.push(CompactedFailed {
            obligation: [8; 32],
            attempt_id: attempt,
            value: Decimal::parse("0.01").unwrap(),
        });
        let at = t("2026-03-01T00:00:00.000Z");
        for claim in [
            None,
            Some(ClaimState::Failed { by_owner: true }),
            Some(ClaimState::Failed { by_owner: false }),
        ] {
            for never_dispatched in [false, true] {
                let mut evidence = HashMap::from([(
                    attempt,
                    AttemptEvidence {
                        claim,
                        never_dispatched,
                        paid: false,
                    },
                )]);
                let used = super::usage(&terms, &l, &evidence, at).unwrap();
                assert_eq!(
                    used.lifetime().as_str(),
                    if never_dispatched { "0" } else { "0.01" }
                );
                evidence.get_mut(&attempt).unwrap().paid = true;
                assert_eq!(
                    super::usage(&terms, &l, &evidence, at)
                        .unwrap()
                        .lifetime()
                        .as_str(),
                    "0.01"
                );
            }
        }
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
        assert_eq!(u.periods()[0].count(), 1);
        assert_eq!(u.lifetime().as_str(), "0.02");
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
    fn test_check_limits_refuses_usage_of_other_limits() {
        // One zero amount ceiling.
        let terms = AllowanceTerms::from_json(&with_limits(json!([{ "amount_limit": "0",
            "payment_count_limit": null,
            "period": { "every": 1, "kind": "rolling", "unit": "day" } }])))
        .unwrap();
        let now = t("2026-02-10T00:00:00.000Z");
        let u = usage(&terms, &ledger(Vec::new()), &HashMap::new(), now).unwrap();
        // Within the per-payment range, so only the period limit decides.
        let one = Decimal::parse("0.005").unwrap();
        assert_eq!(check_limits(&terms, &u, &one), Err(LimitKind::PeriodAmount));
        // Shorter: the zero ceiling left out isn't skipped.
        let short = Usage {
            periods: Vec::new(),
            ..u.clone()
        };
        assert_eq!(
            check_limits(&terms, &short, &one),
            Err(LimitKind::InconsistentUsage)
        );
        // Longer, and usage of other terms with as many limits.
        let long = Usage {
            periods: vec![u.periods[0].clone(), u.periods[0].clone()],
            ..u.clone()
        };
        assert_eq!(
            check_limits(&terms, &long, &one),
            Err(LimitKind::InconsistentUsage)
        );
        let other = AllowanceTerms::from_json(&with_limits(json!([{ "amount_limit": "5",
            "payment_count_limit": null,
            "period": { "every": 1, "kind": "rolling", "unit": "day" } }])))
        .unwrap();
        let other_usage = usage(&other, &ledger(Vec::new()), &HashMap::new(), now).unwrap();
        assert_eq!(
            check_limits(&terms, &other_usage, &one),
            Err(LimitKind::InconsistentUsage)
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
    fn test_ledger_writes_spent_normalized() {
        let l = Ledger {
            spent: Some(Decimal::parse("00.0100").unwrap()),
            count: 1,
            ..ledger(Vec::new())
        };
        let json = l.to_json().unwrap();
        let back: Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(back["totals"]["spent"], "0.01");
        assert_eq!(Ledger::from_json(&json).unwrap(), l);
        let zero = Ledger {
            spent: Some(Decimal::parse("0.000").unwrap()),
            ..ledger(Vec::new())
        };
        let back: Value = serde_json::from_slice(&zero.to_json().unwrap()).unwrap();
        assert_eq!(back["totals"]["spent"], "0");
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
        // The writer refuses what the reader refuses: an attempt listed twice.
        assert!(matches!(twice.to_json(), Err(Error::Validation(_))));
        assert!(Ledger::from_json(&twice.json_unchecked().unwrap()).is_err());
        assert!(Ledger::from_json(br#"{"v":2,"counterparty":"x","allowance_id":"y"}"#).is_err());
    }

    #[test]
    fn test_record_outcomes_copies_paid_and_owner_failed_attempts_once() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let mut held = ledger(vec![
            entry(1, "0.01", "2026-03-01T00:00:00.000Z", None),
            entry(2, "0.02", "2026-03-01T00:01:00.000Z", None),
            entry(
                3,
                "0.03",
                "2026-03-01T00:02:00.000Z",
                Some(EntryOutcome::Failed),
            ),
            entry(4, "0.04", "2026-03-01T00:03:00.000Z", None),
        ]);
        let id = |n| AttemptId::from_bytes([n; 16]);
        held.failed.push(CompactedFailed {
            obligation: [5; 32],
            attempt_id: id(5),
            value: Decimal::new("0.05").unwrap(),
        });
        let proof = AttemptEvidence {
            paid: true,
            ..Default::default()
        };
        let evidence = HashMap::from([
            (id(1), ClaimState::Done.into()),
            (id(2), ClaimState::Failed { by_owner: true }.into()),
            (id(3), proof),
            (id(4), ClaimState::Failed { by_owner: false }.into()),
            (id(5), proof),
        ]);
        let next = record_outcomes(&terms, &held, &evidence).unwrap().unwrap();
        assert_eq!(
            next.entries
                .iter()
                .map(|entry| entry.outcome)
                .collect::<Vec<_>>(),
            [
                Some(EntryOutcome::Done),
                Some(EntryOutcome::Failed),
                Some(EntryOutcome::Done),
                None
            ]
        );
        assert!(next.failed.is_empty());
        assert_eq!(next.spent.as_ref().unwrap().as_str(), "0.05");
        assert_eq!(next.count, 1);
        assert!(record_outcomes(&terms, &next, &evidence).unwrap().is_none());
        assert!(record_outcomes(&terms, &held, &HashMap::new())
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_successor_moves_a_compacted_failed_into_the_totals_once() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let item = CompactedFailed {
            obligation: [7; 32],
            attempt_id: AttemptId::from_bytes([7; 16]),
            value: Decimal::parse("1").unwrap(),
        };
        let held = Ledger {
            failed: vec![item.clone()],
            ..ledger(Vec::new())
        };
        let moved = Ledger {
            spent: Some(Decimal::parse("1").unwrap()),
            count: 1,
            ..ledger(Vec::new())
        };
        let now = t("2026-03-10T00:00:00.000Z");
        check_successor(&terms, &held, now, &moved, now).unwrap();
        // The same attempt also back as an entry would count twice.
        let twice = Ledger {
            entries: vec![LedgerEntry {
                outcome: Some(EntryOutcome::Done),
                ..entry(7, "1", "2026-01-05T00:00:00.000Z", None)
            }],
            ..moved
        };
        let error = check_successor(&terms, &held, now, &twice, now).unwrap_err();
        assert!(
            error.to_string().contains("keeps it as an entry"),
            "{error}"
        );
    }

    #[test]
    fn test_ledger_valid_lists_each_attempt_once() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let held = ledger(Vec::new());
        let twice = Ledger {
            failed: vec![CompactedFailed {
                obligation: [1; 32],
                attempt_id: AttemptId::from_bytes([1; 16]),
                value: Decimal::parse("0.01").unwrap(),
            }],
            ..ledger(vec![entry(1, "0.01", "2026-02-10T00:00:00.000Z", None)])
        };
        let now = t("2026-02-10T00:00:00.000Z");
        assert!(twice.check_valid(&terms).is_err());
        assert!(usage(&terms, &twice, &HashMap::new(), now).is_err());
        assert!(compact(&terms, &twice, now).is_err());
        let error = check_successor(&terms, &held, now, &twice, now).unwrap_err();
        assert!(error.to_string().contains("listed twice"), "{error}");
    }

    #[test]
    fn test_successor_is_the_same_allowances_ledger() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let now = t("2026-03-10T00:00:00.000Z");
        let empty = ledger(Vec::new());
        check_successor(&terms, &empty, now, &empty, now).unwrap();
        let other_counterparty = Ledger {
            counterparty: [6; 32],
            ..empty.clone()
        };
        let other_allowance = Ledger {
            allowance_id: AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap(),
            ..empty.clone()
        };
        let other_acceptance = Ledger {
            acceptance: [7; 32],
            ..empty.clone()
        };
        for newer in [other_counterparty, other_allowance, other_acceptance] {
            let error = check_successor(&terms, &empty, now, &newer, now).unwrap_err();
            assert!(error.to_string().contains("another Allowance"), "{error}");
        }
        // Non-empty too.
        let held = ledger(vec![entry(1, "0.01", "2026-03-01T00:00:00.000Z", None)]);
        let moved = Ledger {
            counterparty: [6; 32],
            ..held.clone()
        };
        assert!(check_successor(&terms, &held, now, &moved, now).is_err());
    }

    #[test]
    fn test_successor_refuses_each_rule() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let now = t("2026-03-10T00:00:00.000Z");
        let failed_item = |id: u8, value: &str| CompactedFailed {
            obligation: [id; 32],
            attempt_id: AttemptId::from_bytes([id; 16]),
            value: Decimal::parse(value).unwrap(),
        };
        let held = Ledger {
            failed: vec![failed_item(9, "0.02")],
            spent: Some(Decimal::parse("0.05").unwrap()),
            count: 2,
            ..ledger(vec![
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
            ])
        };
        check_successor(&terms, &held, now, &held, now).unwrap();
        let mut changed = held.clone();
        changed.entries[0].value = Decimal::parse("0.001").unwrap();
        let mut done_failed = held.clone();
        done_failed.entries.remove(0);
        done_failed.failed.push(failed_item(1, "0.01"));
        let mut other_value = held.clone();
        other_value.entries.remove(1);
        other_value.failed.push(failed_item(2, "0.03"));
        let mut item_changed = held.clone();
        item_changed.failed[0].value = Decimal::parse("0.01").unwrap();
        let mut item_dropped = held.clone();
        item_dropped.failed.clear();
        let mut count_short = held.clone();
        count_short.entries.remove(0);
        count_short.spent = Some(Decimal::parse("0.06").unwrap());
        for (name, newer, rule) in [
            ("an entry's value changed", changed, "changes an entry"),
            (
                "a done entry into totals.failed",
                done_failed,
                "moves a done entry into totals.failed",
            ),
            (
                "a compacted entry with another value",
                other_value,
                "another obligation or value",
            ),
            (
                "a totals.failed item changed",
                item_changed,
                "changes a totals.failed item",
            ),
            (
                "a totals.failed item dropped",
                item_dropped,
                "doesn't grow spent and count",
            ),
            (
                "an entry into spent but not count",
                count_short,
                "doesn't grow spent and count",
            ),
        ] {
            let error = check_successor(&terms, &held, now, &newer, now).unwrap_err();
            assert!(error.to_string().contains(rule), "{name}: {error}");
        }
        // Sums that overflow fail closed, in a newer version and in compaction.
        let full = Ledger {
            spent: Some(Decimal::from_scaled(u128::MAX)),
            ..held.clone()
        };
        let mut over = full.clone();
        over.entries.remove(0);
        assert!(check_successor(&terms, &full, now, &over, now).is_err());
        assert!(compact(&terms, &full, now).is_err());
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
        check_successor(&terms, &before, now, &after, now).unwrap();
        let early = t("2026-01-10T00:00:00.000Z");
        assert!(check_successor(&terms, &before, early, &after, early).is_err());
        let mut flat = after.clone();
        flat.spent = None;
        assert!(check_successor(&terms, &before, now, &flat, now).is_err());
        let mut back = before.clone();
        back.entries[0].outcome = Some(EntryOutcome::Failed);
        assert!(check_successor(&terms, &before, now, &back, now).is_err());
    }

    /// Terms with one anchored limit in `unit` from 2026-01-01, a held ledger with one `done`
    /// entry at `at`, and the version a compaction at `compacted_at` writes from it.
    fn compacted_between(
        unit: &str,
        at: &str,
        compacted_at: &str,
    ) -> (AllowanceTerms, Ledger, Ledger) {
        let terms = AllowanceTerms::from_json(&with_limits(json!([{ "amount_limit": "0.03",
            "payment_count_limit": 3, "period": { "anchor": "2026-01-01T00:00:00.000Z",
                "every": 1, "kind": "anchored", "unit": unit } }])))
        .unwrap();
        let held = ledger(vec![entry(1, "0.01", at, Some(EntryOutcome::Done))]);
        let newer = compact(&terms, &held, t(compacted_at)).unwrap().unwrap();
        assert!(newer.entries.is_empty());
        (terms, held, newer)
    }

    #[test]
    fn test_successor_month_boundary_accepts_a_compaction_in_the_shorter_month() {
        // February's 28-day period lets a write at its end compact the entry; March's 31 days
        // would keep it at the newer write's time.
        let (terms, held, newer) = compacted_between(
            "month",
            "2026-01-31T12:00:00.000Z",
            "2026-02-28T23:59:00.000Z",
        );
        let held_time = t("2026-01-31T13:00:00.000Z");
        let newer_time = t("2026-03-01T00:00:30.000Z");
        check_successor(&terms, &held, held_time, &newer, newer_time).unwrap();
        // Judged at the newer write's time alone, the same step is refused.
        let error = check_successor(&terms, &held, newer_time, &newer, newer_time).unwrap_err();
        assert!(
            error.to_string().contains("before it could be compacted"),
            "{error}"
        );
        // The last millisecond of February is still in the range; the first of March alone
        // isn't enough.
        let feb_end = t("2026-02-28T23:59:59.999Z");
        check_successor(&terms, &held, feb_end, &newer, newer_time).unwrap();
        assert!(check_successor(&terms, &held, feb_end + 1, &newer, newer_time).is_err());
    }

    #[test]
    fn test_successor_leap_year_accepts_a_compaction_in_the_shorter_year() {
        // 2027 has 365 days, leap year 2028 has 366.
        let (terms, held, newer) = compacted_between(
            "year",
            "2026-12-31T12:00:00.000Z",
            "2027-12-31T23:00:00.000Z",
        );
        let held_time = t("2026-12-31T13:00:00.000Z");
        let newer_time = t("2028-01-01T00:00:30.000Z");
        check_successor(&terms, &held, held_time, &newer, newer_time).unwrap();
        assert!(check_successor(&terms, &held, newer_time, &newer, newer_time).is_err());
    }

    #[test]
    fn test_successor_refuses_an_entry_no_time_between_could_compact() {
        // At every time from February 2 to March 1 01:00 the entry is younger than the
        // period then: 28 days in February, 31 in March. Only a later write may compact it.
        let (terms, held, newer) = compacted_between(
            "month",
            "2026-02-01T00:30:00.000Z",
            "2026-04-15T00:00:00.000Z",
        );
        let held_time = t("2026-02-02T00:00:00.000Z");
        let error = check_successor(
            &terms,
            &held,
            held_time,
            &newer,
            t("2026-03-01T01:00:00.000Z"),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("before it could be compacted"),
            "{error}"
        );
        check_successor(
            &terms,
            &held,
            held_time,
            &newer,
            t("2026-03-04T00:31:00.001Z"),
        )
        .unwrap();
    }

    #[test]
    fn test_successor_held_time_after_newer_time_judges_at_newer_time() {
        let (terms, held, newer) = compacted_between(
            "month",
            "2026-01-31T12:00:00.000Z",
            "2026-02-28T23:59:00.000Z",
        );
        // Nothing from March 1 to March 5 is judged: at March 5 the entry would be old
        // enough, at the newer write's time it isn't.
        let newer_time = t("2026-03-01T00:00:30.000Z");
        let held_time = t("2026-03-05T00:00:00.000Z");
        assert!(check_successor(&terms, &held, held_time, &newer, newer_time).is_err());
        let newer_time = t("2026-03-03T12:01:00.001Z");
        check_successor(&terms, &held, held_time, &newer, newer_time).unwrap();
        assert!(check_successor(&terms, &held, held_time, &newer, newer_time - 1).is_err());
    }

    #[test]
    fn test_ledger_keeps_unknown_members() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let held = ledger(vec![
            entry(
                1,
                "0.01",
                "2026-01-05T00:00:00.000Z",
                Some(EntryOutcome::Done),
            ),
            entry(2, "0.01", "2026-03-05T00:00:00.000Z", None),
        ]);
        let mut value: Value = serde_json::from_slice(&held.to_json().unwrap()).unwrap();
        value["note"] = json!({ "by": "a later version" });
        value["totals"]["carried"] = json!("0.03");
        value["entries"][0]["rail"] = json!("lightning");
        value["entries"][1]["rail"] = json!("on-chain");
        let read = Ledger::from_map(value.as_object().unwrap()).unwrap();
        assert!(!read.unknown.is_empty());
        let out: Value = serde_json::from_slice(&read.to_json().unwrap()).unwrap();
        assert_eq!(out, value);
        // Compaction drops the first entry and its members; the others stay.
        let now = t("2026-03-10T00:00:00.000Z");
        let compacted = compact(&terms, &read, now).unwrap().unwrap();
        let out: Value = serde_json::from_slice(&compacted.to_json().unwrap()).unwrap();
        assert_eq!(out["note"], value["note"]);
        assert_eq!(out["totals"]["carried"], "0.03");
        assert_eq!(out["entries"].as_array().unwrap().len(), 1);
        assert_eq!(out["entries"][0]["rail"], "on-chain");
        assert!(!out.to_string().contains("lightning"));
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

    #[test]
    fn test_terms_take_a_rolling_period_too_long_to_evaluate() {
        let long = with_limits(json!([{ "amount_limit": "1", "payment_count_limit": null,
            "period": { "every": 30_500_568_905u64, "kind": "rolling", "unit": "week" } }]));
        // Structurally valid: it parses and writes back as signed.
        let terms = AllowanceTerms::from_json(&long).unwrap();
        assert_eq!(terms.to_json().unwrap(), long);
        // Its length overflows, so every evaluation that needs it is ineligible.
        let l = ledger(vec![entry(1, "0.01", "2026-02-10T00:00:00.000Z", None)]);
        let now = t("2026-02-10T00:00:00.000Z");
        assert!(usage(&terms, &l, &HashMap::new(), now).is_err());
        assert!(usage(&terms, &ledger(Vec::new()), &HashMap::new(), now).is_err());
        assert!(terms.longest_period(now).is_err());
        assert!(compact(&terms, &l, now).is_err());
        assert!(check_successor(&terms, &l, now, &l, now).is_err());
        // A rolling month stays invalid.
        let month = with_limits(json!([{ "amount_limit": "1", "payment_count_limit": null,
            "period": { "every": 1, "kind": "rolling", "unit": "month" } }]));
        assert!(AllowanceTerms::from_json(&month).is_err());
    }

    #[test]
    fn test_usage_fails_closed_when_a_rolling_window_end_overflows() {
        // L fits in 64 bits, but an entry's at + 60 s + L doesn't.
        let every = 30_500_568_904u64;
        assert!(Unit::Week
            .fixed_ms()
            .and_then(|w| w.checked_mul(every))
            .is_some());
        let terms = AllowanceTerms::from_json(&with_limits(json!([{ "amount_limit": "1",
            "payment_count_limit": null,
            "period": { "every": every, "kind": "rolling", "unit": "week" } }])))
        .unwrap();
        let now = t("2026-02-10T00:00:00.000Z");
        let l = ledger(vec![entry(1, "0.01", "2026-02-10T00:00:00.000Z", None)]);
        let error = usage(&terms, &l, &HashMap::new(), now).unwrap_err();
        assert!(
            error.to_string().contains("can't be represented"),
            "{error}"
        );
        // Without entries, the candidate's own window at t ends past what can be represented
        // too, so the evaluation is still ineligible.
        assert!(usage(&terms, &ledger(Vec::new()), &HashMap::new(), now).is_err());
    }

    #[test]
    fn test_compaction_fails_closed_when_an_entry_bound_overflows() {
        // L fits, but at + L + 60 s doesn't: the entry can't be judged old enough or not.
        let terms = AllowanceTerms::from_json(&with_limits(json!([{ "amount_limit": "1",
            "payment_count_limit": null,
            "period": { "every": 30_500_568_904u64, "kind": "rolling", "unit": "week" } }])))
        .unwrap();
        let now = t("2026-03-10T00:00:00.000Z");
        let held = ledger(vec![entry(
            1,
            "0.01",
            "2026-01-05T00:00:00.000Z",
            Some(EntryOutcome::Done),
        )]);
        let error = compact(&terms, &held, now).unwrap_err();
        assert!(
            error.to_string().contains("can't be represented"),
            "{error}"
        );
        let compacted = Ledger {
            spent: Some(Decimal::parse("0.01").unwrap()),
            count: 1,
            ..ledger(Vec::new())
        };
        assert!(check_successor(&terms, &held, now, &compacted, now).is_err());
    }

    #[test]
    fn test_evaluation_refuses_terms_and_ledgers_no_reader_takes() {
        let now = t("2026-02-10T00:00:00.000Z");
        let terms = AllowanceTerms::from_json(&with_limits(json!([{ "amount_limit": "0.01",
            "payment_count_limit": null,
            "period": { "every": 1, "kind": "rolling", "unit": "day" } }])))
        .unwrap();
        // A rolling every of 0, which no reader takes, would make L zero.
        let mut zero = terms.clone();
        zero.period_limits[0].period = LimitPeriod::Rolling {
            every: 0,
            unit: Unit::Day,
        };
        let l = ledger(vec![entry(1, "0.01", "2026-02-09T23:00:00.000Z", None)]);
        usage(&terms, &l, &HashMap::new(), now).unwrap();
        assert!(matches!(
            usage(&zero, &l, &HashMap::new(), now),
            Err(Error::Validation(_))
        ));
        // An entry dated past the time grammar.
        let mut late = l.clone();
        late.entries[0].at = u64::MAX;
        assert!(usage(&terms, &late, &HashMap::new(), now).is_err());
    }

    #[test]
    fn test_anchored_limit_error_class_follows_its_source() {
        let anchor = TermsTime::new(t("2026-01-31T00:00:00Z"));
        assert!(matches!(
            AnchoredLimit::new(anchor.clone(), 0, Unit::Month),
            Err(Error::Validation(_))
        ));
        assert!(matches!(
            AnchoredLimit::read(anchor, 0, Unit::Month),
            Err(Error::Invalid(_))
        ));
        let terms = with_limits(json!([{ "amount_limit": "1", "payment_count_limit": null,
            "period": { "anchor": "2026-01-31T00:00:00Z", "every": 0, "kind": "anchored",
            "unit": "month" } }]));
        assert!(matches!(
            AllowanceTerms::from_json(&terms),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn test_entry_window_stays_within_the_time_grammar() {
        let terms = AllowanceTerms::from_json(&with_limits(json!([{ "amount_limit": "1",
            "payment_count_limit": null,
            "period": { "every": 1, "kind": "rolling", "unit": "day" } }])))
        .unwrap();
        let last = MAX_TIME_MS - ENTRY_WINDOW_MS;
        assert_eq!(terms.entry_window_end(last).unwrap(), MAX_TIME_MS);
        for at in [last + 1, MAX_TIME_MS, MAX_TIME_MS + 1, u64::MAX] {
            assert!(
                matches!(terms.entry_window_end(at), Err(Error::Validation(_))),
                "{at}"
            );
        }
    }
    const IN_MONTH: &str = "2026-03-05T00:00:00.000Z";

    /// What a repair merges: a kept version, the ledger shown, the journal's attempts and the
    /// claims', overlapping, with entries of every outcome, compacted totals, and a shown
    /// entry of another asset. Amounts and the complete accepted checkpoint are supplied
    /// fixtures; their authority is a caller prerequisite, not proved by these tests.
    fn repair_inputs() -> (Ledger, Ledger, Vec<LedgerEntry>, Vec<LedgerEntry>) {
        let mut kept = ledger(vec![
            entry(1, "0.01", IN_MONTH, Some(EntryOutcome::Done)),
            entry(2, "0.01", IN_MONTH, None),
            entry(3, "0.01", IN_MONTH, Some(EntryOutcome::Failed)),
        ]);
        kept.failed.push(CompactedFailed {
            obligation: [4; 32],
            attempt_id: AttemptId::from_bytes([4; 16]),
            value: Decimal::parse("0.01").unwrap(),
        });
        let mut other_asset = entry(6, "0.01", IN_MONTH, None);
        other_asset.asset = Asset::parse("usd").unwrap();
        let mut shown = ledger(vec![
            entry(2, "0.01", IN_MONTH, Some(EntryOutcome::Done)),
            entry(
                4,
                "0.01",
                "2025-06-01T00:00:00.000Z",
                Some(EntryOutcome::Done),
            ),
            entry(5, "0.02", IN_MONTH, Some(EntryOutcome::Failed)),
            other_asset,
        ]);
        shown.failed.push(CompactedFailed {
            obligation: [7; 32],
            attempt_id: AttemptId::from_bytes([7; 16]),
            value: Decimal::parse("0.01").unwrap(),
        });
        shown.spent = Some(Decimal::parse("0.02").unwrap());
        shown.count = 2;
        let journal = vec![
            entry(2, "0.01", IN_MONTH, None),
            entry(8, "0.01", IN_MONTH, None),
        ];
        let claims = vec![
            entry(8, "0.01", IN_MONTH, None),
            entry(9, "0.01", IN_MONTH, None),
        ];
        (kept, shown, journal, claims)
    }

    /// Claims as an executor may read them for the repair's attempts: gone, open, and each
    /// outcome, the kept `failed` borne out by a `failed` claim or a later `done`.
    fn repair_claims() -> Vec<HashMap<AttemptId, ClaimState>> {
        let id = |n: u8| AttemptId::from_bytes([n; 16]);
        let mut open = HashMap::new();
        for n in [2, 5, 8, 9] {
            open.insert(id(n), ClaimState::Open);
        }
        open.insert(id(3), ClaimState::Failed { by_owner: true });
        let mut settled = HashMap::new();
        for n in [2, 3, 4, 5, 7, 8, 9] {
            settled.insert(id(n), ClaimState::Done);
        }
        let mut failed = HashMap::new();
        for n in [2, 5, 8] {
            failed.insert(id(n), ClaimState::Failed { by_owner: false });
        }
        failed.insert(id(9), ClaimState::Failed { by_owner: true });
        failed.insert(id(3), ClaimState::Failed { by_owner: false });
        vec![HashMap::new(), open, settled, failed]
    }

    fn repair_unknown_inputs() -> (Value, Value) {
        let mut kept = ledger(vec![entry(1, "0.01", IN_MONTH, None)]);
        kept.failed.push(CompactedFailed {
            obligation: [2; 32],
            attempt_id: AttemptId::from_bytes([2; 16]),
            value: Decimal::parse("0.01").unwrap(),
        });
        let mut shown = kept.clone();
        shown.entries.push(entry(3, "0.01", IN_MONTH, None));
        shown.failed.push(CompactedFailed {
            obligation: [4; 32],
            attempt_id: AttemptId::from_bytes([4; 16]),
            value: Decimal::parse("0.01").unwrap(),
        });
        (
            serde_json::from_slice(&kept.to_json().unwrap()).unwrap(),
            serde_json::from_slice(&shown.to_json().unwrap()).unwrap(),
        )
    }

    fn repair_unknown(kept: Value, shown: Value, own: &[LedgerEntry]) -> Result<Value> {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let kept = Ledger::from_map(kept.as_object().unwrap()).unwrap();
        let shown = Ledger::from_map(shown.as_object().unwrap()).unwrap();
        let merged = merge_for_repair(&terms, &kept, Some(&shown), own, &[], t(IN_MONTH))?;
        Ok(serde_json::from_slice(&merged.to_json()?).unwrap())
    }

    fn repair_keeps_unknown_at(path: &str) {
        let (mut kept, mut shown) = repair_unknown_inputs();
        kept.pointer_mut(path).unwrap()["kept_note"] = json!({"kept": [1, null]});
        shown.pointer_mut(path).unwrap()["shown_note"] = json!({"shown": [2, true]});
        let out = repair_unknown(kept, shown, &[]).unwrap();
        assert_eq!(
            out.pointer(path).unwrap()["kept_note"],
            json!({"kept": [1, null]})
        );
        assert_eq!(
            out.pointer(path).unwrap()["shown_note"],
            json!({"shown": [2, true]})
        );
    }

    fn repair_refuses_unknown_conflict_at(path: &str) {
        let (mut kept, mut shown) = repair_unknown_inputs();
        kept.pointer_mut(path).unwrap()["note"] = json!({"kept": 1});
        shown.pointer_mut(path).unwrap()["note"] = json!({"shown": 2});
        assert!(matches!(
            repair_unknown(kept, shown, &[]),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn test_merge_for_repair_preserves_unknown_root_members() {
        repair_keeps_unknown_at("");
    }

    #[test]
    fn test_merge_for_repair_preserves_unknown_totals_members() {
        repair_keeps_unknown_at("/totals");
    }

    #[test]
    fn test_merge_for_repair_preserves_unknown_entry_members() {
        repair_keeps_unknown_at("/entries/0");
    }

    #[test]
    fn test_merge_for_repair_preserves_unknown_failed_members() {
        repair_keeps_unknown_at("/totals/failed/0");
    }

    #[test]
    fn test_merge_for_repair_refuses_unknown_root_conflict() {
        repair_refuses_unknown_conflict_at("");
    }

    #[test]
    fn test_merge_for_repair_refuses_unknown_totals_conflict() {
        repair_refuses_unknown_conflict_at("/totals");
    }

    #[test]
    fn test_merge_for_repair_refuses_unknown_entry_conflict() {
        repair_refuses_unknown_conflict_at("/entries/0");
    }

    #[test]
    fn test_merge_for_repair_refuses_unknown_failed_conflict() {
        repair_refuses_unknown_conflict_at("/totals/failed/0");
    }

    #[test]
    fn test_merge_for_repair_preserves_imported_unknown_members() {
        let (kept, mut shown) = repair_unknown_inputs();
        shown["entries"][1]["note"] = json!("entry");
        shown["totals"]["failed"][1]["note"] = json!("failed");
        let out = repair_unknown(kept, shown, &[]).unwrap();
        assert_eq!(out["entries"][1]["note"], "entry");
        assert_eq!(out["totals"]["failed"][1]["note"], "failed");
    }

    #[test]
    fn test_merge_for_repair_preserves_imported_failed_unknown_members() {
        let (kept, mut shown) = repair_unknown_inputs();
        shown["totals"]["failed"][1]["note"] = json!("failed");
        let out = repair_unknown(kept, shown, &[]).unwrap();
        assert_eq!(out["totals"]["failed"][1]["note"], "failed");
    }

    #[test]
    fn test_merge_for_repair_does_not_take_members_from_an_invalid_entry() {
        let (kept, mut shown) = repair_unknown_inputs();
        shown["entries"][0]["asset"] = json!("usd");
        shown["entries"][0]["note"] = json!("another asset");
        let out = repair_unknown(kept, shown, &[]).unwrap();
        assert_eq!(out["entries"][0]["asset"], "btc");
        assert!(out["entries"][0].get("note").is_none());
    }

    #[test]
    fn test_ledger_compaction_drops_the_last_unknown_object() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let held = ledger(vec![entry(
            1,
            "0.01",
            "2026-01-05T00:00:00.000Z",
            Some(EntryOutcome::Done),
        )]);
        let mut input: Value = serde_json::from_slice(&held.to_json().unwrap()).unwrap();
        input["entries"][0]["note"] = json!("removed entry");
        let before = Ledger::from_map(input.as_object().unwrap()).unwrap();
        let after = compact(&terms, &before, t(IN_MONTH)).unwrap().unwrap();
        assert!(after.entries.is_empty());
        let out: Value = serde_json::from_slice(&after.to_json().unwrap()).unwrap();
        assert!(!out.to_string().contains("removed entry"));
        assert_eq!(out["totals"]["spent"], "0.01");
    }

    #[test]
    fn test_merge_for_repair_keeps_identical_unknown_values() {
        let (mut kept, mut shown) = repair_unknown_inputs();
        for path in ["", "/totals", "/entries/0", "/totals/failed/0"] {
            kept.pointer_mut(path).unwrap()["note"] = json!({"same": [null, false]});
            shown.pointer_mut(path).unwrap()["note"] = json!({"same": [null, false]});
        }
        let out = repair_unknown(kept.clone(), shown, &[]).unwrap();
        for path in ["", "/totals", "/entries/0", "/totals/failed/0"] {
            assert_eq!(
                out.pointer(path).unwrap()["note"],
                kept.pointer(path).unwrap()["note"]
            );
        }
    }

    #[test]
    fn test_merge_for_repair_drops_unknown_members_with_replaced_objects() {
        let (mut kept, mut shown) = repair_unknown_inputs();
        kept["totals"]["failed"][0]["note"] = json!("kept failed");
        shown["totals"]["failed"][0]["note"] = json!("shown failed");
        shown["totals"]["failed"][1]["note"] = json!("replaced failed");
        shown["entries"][1]["asset"] = json!("usd");
        shown["entries"][1]["note"] = json!("discarded asset");
        let own = [
            entry(2, "0.01", IN_MONTH, Some(EntryOutcome::Done)),
            entry(4, "0.01", IN_MONTH, None),
        ];
        let out = repair_unknown(kept, shown, &own).unwrap();
        assert!(out["totals"].get("failed").is_none());
        assert_eq!(out["entries"].as_array().unwrap().len(), 2);
        assert!(out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry.get("note").is_none()));
    }

    #[test]
    fn test_merge_for_repair_never_counts_less_than_any_input() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let (kept, shown, journal, claims) = repair_inputs();
        let merged =
            merge_for_repair(&terms, &kept, Some(&shown), &journal, &claims, t(IN_MONTH)).unwrap();
        // The ledger shown as usage reads it: without its entry of another asset.
        let mut shown_valid = shown.clone();
        shown_valid.entries.retain(|e| e.asset == terms.asset);
        let inputs = [
            kept.clone(),
            shown_valid,
            ledger(journal.clone()),
            ledger(claims.clone()),
            Ledger::empty(
                kept.counterparty,
                kept.allowance_id.clone(),
                kept.acceptance,
            ),
        ];
        let times = [
            IN_MONTH,
            "2026-03-20T00:00:00.000Z",
            "2026-05-01T00:00:00.000Z",
        ];
        assert_counts_no_less(&terms, &merged, &inputs, &times, &repair_claims());
        for at in times {
            check_successor(&terms, &kept, t(at), &merged, t(at)).unwrap();
        }
    }

    #[test]
    fn test_merge_for_repair_takes_what_the_ledger_shown_adds() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let (kept, shown, journal, claims) = repair_inputs();
        let merged =
            merge_for_repair(&terms, &kept, Some(&shown), &journal, &claims, t(IN_MONTH)).unwrap();
        let outcome = |n: u8| {
            merged
                .entries
                .iter()
                .find(|e| e.attempt_id == AttemptId::from_bytes([n; 16]))
                .map(|e| e.outcome)
        };
        // The kept entries as they were, a `done` shown added; the kept `failed` stays.
        assert_eq!(outcome(1), Some(Some(EntryOutcome::Done)));
        assert_eq!(outcome(2), Some(Some(EntryOutcome::Done)));
        assert_eq!(outcome(3), Some(Some(EntryOutcome::Failed)));
        // A `failed` shown isn't taken; the entry of another asset is left out.
        assert_eq!(outcome(5), Some(None));
        assert_eq!(outcome(6), None);
        // The journal's and the claims' attempts, once each.
        assert_eq!(outcome(8), Some(None));
        assert_eq!(outcome(9), Some(None));
        assert_eq!(merged.entries.len(), 6);
        // The compacted failed attempt shown paid moves into the higher totals, once; the
        // one shown compacted comes along.
        assert!(merged
            .failed
            .iter()
            .all(|f| f.attempt_id != AttemptId::from_bytes([4; 16])));
        assert!(merged
            .failed
            .iter()
            .any(|f| f.attempt_id == AttemptId::from_bytes([7; 16])));
        assert_eq!(merged.spent.as_ref().map(Decimal::as_str), Some("0.03"));
        assert_eq!(merged.count, 3);
        // Without a ledger shown: the kept version, the journal's and the claims' attempts.
        let alone = merge_for_repair(&terms, &kept, None, &journal, &claims, t(IN_MONTH)).unwrap();
        assert_eq!(alone.entries.len(), 5);
        assert_eq!(alone.failed, kept.failed);
        assert_eq!((alone.spent.clone(), alone.count), (None, 0));
        // A qualified empty checkpoint plus the journal and claim-derived entries.
        let empty = Ledger::empty(
            kept.counterparty,
            kept.allowance_id.clone(),
            kept.acceptance,
        );
        let fresh = merge_for_repair(&terms, &empty, None, &journal, &claims, t(IN_MONTH)).unwrap();
        assert_eq!(fresh.entries.len(), 3);
    }

    #[test]
    fn test_merge_for_repair_refuses_another_allowances_ledger() {
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let (kept, mut shown, journal, claims) = repair_inputs();
        shown.counterparty = [6; 32];
        assert!(
            merge_for_repair(&terms, &kept, Some(&shown), &journal, &claims, t(IN_MONTH)).is_err()
        );
        shown.counterparty = kept.counterparty;
        shown.acceptance = [7; 32];
        assert!(
            merge_for_repair(&terms, &kept, Some(&shown), &journal, &claims, t(IN_MONTH)).is_err()
        );
        let mut invalid_kept = kept.clone();
        invalid_kept.entries[0].asset = Asset::parse("usd").unwrap();
        assert!(
            merge_for_repair(&terms, &invalid_kept, None, &journal, &claims, t(IN_MONTH)).is_err()
        );
    }

    /// Usage over `merged` is no less than over any of `inputs`, by the lifetime amount and
    /// every period limit's amount and count, at each of `times` and with each of `states`.
    fn assert_counts_no_less(
        terms: &AllowanceTerms,
        merged: &Ledger,
        inputs: &[Ledger],
        times: &[&str],
        states: &[HashMap<AttemptId, ClaimState>],
    ) {
        for at in times {
            for states in states {
                let after = usage(terms, merged, states, t(at)).unwrap();
                for (i, input) in inputs.iter().enumerate() {
                    let before = usage(terms, input, states, t(at)).unwrap();
                    assert!(after.lifetime() >= before.lifetime(), "{at} input {i}");
                    for (a, b) in after.periods().iter().zip(before.periods()) {
                        assert!(a.amount() >= b.amount(), "{at} input {i}");
                        assert!(a.count() >= b.count(), "{at} input {i}");
                    }
                }
            }
        }
    }

    /// Terms with a monthly anchored limit and a rolling day, so an entry's time decides
    /// both.
    fn month_and_day_terms() -> AllowanceTerms {
        AllowanceTerms::from_json(&with_limits(json!([
            { "amount_limit": "0.03", "payment_count_limit": 3,
              "period": { "anchor": "2026-01-31T00:00:00.000Z", "every": 1, "kind": "anchored", "unit": "month" } },
            { "amount_limit": "0.02", "payment_count_limit": 2,
              "period": { "kind": "rolling", "every": 1, "unit": "day" } }
        ])))
        .unwrap()
    }

    #[test]
    fn test_merge_for_repair_takes_the_evidence_that_counts_most() {
        // A doubtful copy shown lists an attempt the journal and a claim show open at a tiny
        // value, dated last year, `failed`; another as a compacted failed item; a third paid
        // only on a proof, at another time than the claim's. Each merged entry takes the
        // larger value, the later time, the journal's obligation and a `done` shown by a
        // proof, never the shown `failed`.
        let terms = month_and_day_terms();
        let kept = ledger(vec![entry(1, "0.01", IN_MONTH, None)]);
        let mut low = entry(
            10,
            "0.0001",
            "2025-03-05T00:00:00.000Z",
            Some(EntryOutcome::Failed),
        );
        low.obligation = [0xee; 32];
        let mut shown = ledger(vec![
            // The kept attempt, shown lower and older: the kept entry stands.
            entry(1, "0.0001", "2025-03-05T00:00:00.000Z", None),
            low,
            entry(12, "0.02", "2026-03-04T23:00:00.000Z", None),
        ]);
        shown.failed.push(CompactedFailed {
            obligation: [11; 32],
            attempt_id: AttemptId::from_bytes([11; 16]),
            value: Decimal::parse("0.0001").unwrap(),
        });
        let journal = vec![entry(10, "0.01", IN_MONTH, None)];
        let mut later = entry(10, "0.005", "2026-03-05T00:00:30.000Z", None);
        later.obligation = [0xdd; 32];
        let claims = vec![
            later,
            entry(11, "0.01", IN_MONTH, None),
            entry(
                12,
                "0.01",
                "2026-03-05T00:00:20.000Z",
                Some(EntryOutcome::Done),
            ),
        ];
        let now = t("2026-03-05T00:01:00.000Z");
        let merged = merge_for_repair(&terms, &kept, Some(&shown), &journal, &claims, now).unwrap();
        let find = |n: u8| {
            merged
                .entries
                .iter()
                .find(|e| e.attempt_id == AttemptId::from_bytes([n; 16]))
                .cloned()
                .unwrap()
        };
        assert_eq!(find(1), kept.entries[0]);
        let ten = find(10);
        assert_eq!(ten.value, Decimal::parse("0.01").unwrap());
        assert_eq!(ten.at, t("2026-03-05T00:00:30.000Z"));
        assert_eq!(ten.obligation, [10; 32]);
        assert_eq!(ten.outcome, None);
        let eleven = find(11);
        assert_eq!(eleven.value, Decimal::parse("0.01").unwrap());
        assert!(merged.failed.is_empty());
        let twelve = find(12);
        assert_eq!(twelve.value, Decimal::parse("0.02").unwrap());
        assert_eq!(twelve.at, t("2026-03-05T00:00:20.000Z"));
        assert_eq!(twelve.outcome, Some(EntryOutcome::Done));
        assert_eq!(merged.entries.len(), 4);
        check_successor(&terms, &kept, now, &merged, now).unwrap();
        let inputs = [
            kept.clone(),
            shown.clone(),
            ledger(journal.clone()),
            ledger(claims.clone()),
        ];
        let id = |n: u8| AttemptId::from_bytes([n; 16]);
        let mut states = vec![HashMap::new()];
        for state in [
            ClaimState::Open,
            ClaimState::Unknown,
            ClaimState::Done,
            ClaimState::Failed { by_owner: false },
            ClaimState::Failed { by_owner: true },
        ] {
            states.push(
                [1, 10, 11, 12]
                    .into_iter()
                    .map(|n| (id(n), state))
                    .collect(),
            );
        }
        assert_counts_no_less(
            &terms,
            &merged,
            &inputs,
            &[
                "2026-03-05T00:01:00.000Z",
                "2026-03-05T23:30:00.000Z",
                "2026-03-06T00:00:45.000Z",
                "2026-03-20T00:00:00.000Z",
                "2026-05-01T00:00:00.000Z",
            ],
            &states,
        );
    }

    #[test]
    fn test_merge_for_repair_dates_an_attempt_no_later_than_the_repair() {
        // A shown copy dated after the repair's time counts as at that time: it counts in
        // the period the repair is in, as the claim's own time would.
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let kept = ledger(Vec::new());
        let shown = ledger(vec![entry(3, "0.01", "2026-04-05T00:00:00.000Z", None)]);
        let merged = merge_for_repair(&terms, &kept, Some(&shown), &[], &[], t(IN_MONTH)).unwrap();
        assert_eq!(merged.entries[0].at, t(IN_MONTH));
        let used = usage(&terms, &merged, &HashMap::new(), t(IN_MONTH)).unwrap();
        assert_eq!(used.periods()[0].count(), 1);
    }

    /// Terms whose only period limit is `period`.
    fn with_period(period: Value) -> AllowanceTerms {
        AllowanceTerms::from_json(&with_limits(json!([{ "amount_limit": "0.03",
            "payment_count_limit": 3, "period": period }])))
        .unwrap()
    }

    #[test]
    fn test_merge_for_repair_counts_evidence_within_the_window_kept() {
        // The kept entry is dated T, the executor's stated time; the claim shows the attempt
        // opened at T + 30 s, the service time its batch landed at. The entry stays at T and
        // counts the attempt in a rolling day through T + 60 s + L, past the attempt's own
        // last instant, T + 30 s + L: no gap at any time.
        let day = 86_400_000;
        let terms = with_period(json!({ "kind": "rolling", "every": 1, "unit": "day" }));
        let at = t(IN_MONTH);
        let kept = ledger(vec![entry(1, "0.01", IN_MONTH, None)]);
        let mut claim = entry(1, "0.01", IN_MONTH, None);
        claim.at = at + 30_000;
        let now = at + 60_000;
        let merged = merge_for_repair(&terms, &kept, None, &[], &[claim], now).unwrap();
        assert_eq!(merged.entries[0].at, at);
        let mut open = HashMap::new();
        open.insert(AttemptId::from_bytes([1; 16]), ClaimState::Open);
        let count = |t: u64| usage(&terms, &merged, &open, t).unwrap().periods()[0].count();
        for t in [now, at + 30_000 + day - 1, at + 60_000 + day - 1] {
            assert_eq!(count(t), 1, "at {t}");
        }
        assert_eq!(count(at + 60_000 + day), 0);
    }

    #[test]
    fn test_merge_for_repair_refuses_evidence_after_the_window_kept() {
        // Own evidence dated at the end of the kept entry's window or later isn't of its
        // batch, from the journal or a claim: refused, as the entry can't be moved.
        let terms = with_period(json!({ "kind": "rolling", "every": 1, "unit": "day" }));
        let at = t(IN_MONTH);
        let kept = ledger(vec![entry(1, "0.01", IN_MONTH, None)]);
        let dated = |ms: u64| LedgerEntry {
            at: ms,
            ..entry(1, "0.01", IN_MONTH, None)
        };
        let now = at + 120_000;
        let last = [dated(at + 59_999)];
        assert!(merge_for_repair(&terms, &kept, None, &last, &[], now).is_ok());
        assert!(merge_for_repair(&terms, &kept, None, &[], &last, now).is_ok());
        let past = [dated(at + 60_000)];
        for (journal, claims) in [(&past[..], &[][..]), (&[][..], &past[..])] {
            let refused = merge_for_repair(&terms, &kept, None, journal, claims, now);
            assert!(
                matches!(&refused, Err(Error::Invalid(why)) if why.contains("after the window")),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn test_merge_for_repair_checks_evidence_time_before_clamping() {
        let terms = with_period(json!({ "kind": "rolling", "every": 1, "unit": "day" }));
        let at = t(IN_MONTH);
        let kept = ledger(vec![entry(1, "0.01", IN_MONTH, None)]);
        let evidence = [LedgerEntry {
            at: at + 60_000,
            ..entry(1, "0.01", IN_MONTH, None)
        }];
        // A later service time does not move an accepted entry's executor window.
        // Capping an evidence time at `now` cannot make incompatible evidence fit it.
        for now in [at + 30_000, at + 60_000, at + 90_000] {
            for (journal, claims) in [(&evidence[..], &[][..]), (&[][..], &evidence[..])] {
                let refused = merge_for_repair(&terms, &kept, None, journal, claims, now);
                assert!(
                    matches!(&refused, Err(Error::Invalid(why)) if why.contains("after the window")),
                    "{refused:?}"
                );
            }
        }
    }

    #[test]
    fn test_merge_for_repair_refuses_evidence_past_an_anchored_boundary() {
        // The kept entry's window is cut at the next boundary of an anchored month: evidence
        // just before it is in the entry's period and taken; evidence at it, in the next
        // period, which the entry doesn't count in, is refused.
        let terms = with_period(json!({ "anchor": "2026-03-01T00:00:00.000Z", "every": 1,
            "kind": "anchored", "unit": "month" }));
        let kept_at = "2026-03-31T23:59:50.000Z";
        let boundary = t("2026-04-01T00:00:00.000Z");
        assert_eq!(terms.entry_window_end(t(kept_at)).unwrap(), boundary);
        let kept = ledger(vec![entry(1, "0.01", kept_at, None)]);
        let dated = |ms: u64| LedgerEntry {
            at: ms,
            ..entry(1, "0.01", kept_at, None)
        };
        let now = boundary + 30_000;
        let merged = merge_for_repair(&terms, &kept, None, &[], &[dated(boundary - 1)], now);
        assert_eq!(merged.unwrap().entries[0].at, t(kept_at));
        let refused = merge_for_repair(&terms, &kept, None, &[], &[dated(boundary)], now);
        assert!(
            matches!(&refused, Err(Error::Invalid(why)) if why.contains("after the window")),
            "{refused:?}"
        );
    }

    #[test]
    fn test_merge_for_repair_refuses_own_evidence_above_the_version_kept() {
        // The version kept counts an attempt as accepted; the journal showing it larger
        // can't be counted as both, so the merge refuses rather than count less.
        let terms = AllowanceTerms::from_json(&terms()).unwrap();
        let kept = ledger(vec![entry(1, "0.005", IN_MONTH, None)]);
        let journal = vec![entry(1, "0.01", IN_MONTH, None)];
        let refused = merge_for_repair(&terms, &kept, None, &journal, &[], t(IN_MONTH));
        assert!(
            matches!(&refused, Err(Error::Invalid(why)) if why.contains("larger value")),
            "{refused:?}"
        );
        let mut failed = ledger(Vec::new());
        failed.failed.push(CompactedFailed {
            obligation: [1; 32],
            attempt_id: AttemptId::from_bytes([1; 16]),
            value: Decimal::parse("0.005").unwrap(),
        });
        assert!(merge_for_repair(&terms, &failed, None, &journal, &[], t(IN_MONTH)).is_err());
        // The same value, a `done` shown by a proof: taken.
        let same = vec![entry(1, "0.005", IN_MONTH, Some(EntryOutcome::Done))];
        let merged = merge_for_repair(&terms, &kept, None, &[], &same, t(IN_MONTH)).unwrap();
        assert_eq!(merged.entries[0].outcome, Some(EntryOutcome::Done));
    }
}
