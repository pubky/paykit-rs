//! The bodies of Paykit's kinds (Paykit spec 5, Paykit wire 4.4 to 4.17): check 10 of the
//! recipient checks (Switch wire 5.1, Paykit wire 4.1), and the writers.
//!
//! Readers ignore unknown members (Switch wire 1.1) and read `null` as a missing member.
//! Every member a recipient reads must have its type, and the rules of the kind's section
//! must hold; a body that fails is dropped at check 10. Writers omit absent optional
//! members, except those a table says are always written.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use pubky_switch_core::claims::attempt_deadline;
use pubky_switch_core::enc::{b64, zbase32_encode};
use pubky_switch_core::message::Message;
use pubky_switch_core::time::rfc3339;
use serde_json::{Map, Value};

use crate::allowances::AllowanceTerms;
use crate::amount::{Asset, Decimal, PaymentAmount, Rate};
use crate::derive::settlement_id;
use crate::error::{ensure, invalid, Error, Result};
use crate::identifiers::{
    AllowanceId, AttemptId, PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use crate::json::{bad, Obj};
use crate::kinds::{ExtensionKind, Kind, MessageKind, MAX_RECORDED_KIND_LEN};
use crate::periods::{BillingPeriod, PeriodRef, Recurrence};
use crate::scopes::Label;

/// How long a payer uses a Private Payment List after its inbox accepted it, at most: 30
/// days (Paykit spec 5, Paykit wire 4.13).
pub const PRIVATE_LIST_MAX_AGE_MS: u64 = 30 * 86_400_000;

/// Payment endpoints (Paykit wire 4.4): Payment Endpoint Identifiers to non-empty payloads,
/// at least one. The sending app owns them (`from.app_id`).
pub type PaymentEndpoints = BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>;

/// Read an amount object (Paykit wire 4.4).
pub(crate) fn read_amount(value: &Value, ctx: &str, what: &str) -> Result<PaymentAmount> {
    let obj = Obj::of(value, ctx, what)?;
    Ok(PaymentAmount {
        value: Decimal::parse(obj.req_str("value")?)?,
        asset: Asset::parse(obj.req_str("asset")?)?,
    })
}

/// Write an amount object.
pub(crate) fn amount_json(amount: &PaymentAmount) -> Value {
    let mut map = Map::new();
    map.insert("asset".into(), Value::String(amount.asset.as_str().into()));
    map.insert("value".into(), Value::String(amount.value.as_str().into()));
    Value::Object(map)
}

fn read_endpoints(value: &Value, ctx: &str) -> Result<PaymentEndpoints> {
    let obj = value
        .as_object()
        .ok_or_else(|| bad(ctx, "payment_endpoints", "not an object"))?;
    if obj.is_empty() {
        return Err(bad(ctx, "payment_endpoints", "at least one member"));
    }
    let mut out = PaymentEndpoints::new();
    for (id, payload) in obj {
        let payload = payload
            .as_str()
            .ok_or_else(|| bad(ctx, "payment_endpoints", "a payload isn't a string"))?;
        out.insert(
            PaymentEndpointIdentifier::parse(id)?,
            PaymentEndpointPayload::parse(payload)?,
        );
    }
    Ok(out)
}

fn endpoints_json(endpoints: &PaymentEndpoints) -> Value {
    Value::Object(
        endpoints
            .iter()
            .map(|(id, payload)| {
                (
                    id.as_str().to_owned(),
                    Value::String(payload.as_str().into()),
                )
            })
            .collect(),
    )
}

fn read_rates(value: &Value, ctx: &str) -> Result<Vec<Rate>> {
    let items = value
        .as_array()
        .ok_or_else(|| bad(ctx, "rates", "not an array"))?;
    if items.is_empty() {
        return Err(bad(ctx, "rates", "empty"));
    }
    let mut out: Vec<Rate> = Vec::with_capacity(items.len());
    for item in items {
        let obj = Obj::of(item, ctx, "rates")?;
        let rate = Rate::new(
            Asset::parse(obj.req_str("asset")?)?,
            Decimal::parse(obj.req_str("value")?)?,
        )?;
        if out.iter().any(|r| r.asset() == rate.asset()) {
            return Err(bad(ctx, "rates", "an asset selector twice"));
        }
        out.push(rate);
    }
    Ok(out)
}

/// The asset segment of a rate selector (Paykit wire 4.4, `specs/payment-conversion.md`):
/// `usdt` for both `usdt` and `usdt-polygon`.
fn selector_asset(selector: &str) -> &str {
    selector.split('-').next().unwrap_or(selector)
}

/// Whether `text` is one segment of the conversion convention: non-empty, lower-case ASCII
/// letters and digits (Paykit wire 4.4, `specs/payment-conversion.md`).
fn is_conversion_segment(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// The constraints of conversion terms (Paykit wire 4.4, `specs/payment-conversion.md`), for
/// a request in `asset` whose accepted endpoints are `accepted`, already in the form
/// `<asset>-<rail>-<format>` (Paykit wire 1.4):
///
/// - the requested asset is one segment, lower-case letters and digits;
/// - each fixed rate's selector is one or two such segments, `<asset>` or `<asset>-<rail>`;
/// - each selector matches at least one accepted endpoint: its asset, and its rail when it
///   names one. A selector never adds an endpoint.
///
/// A request that names no endpoint, neither `accepted_payment_endpoint_identifiers` nor
/// `payment_endpoints`, accepts those of the payee's lists, which check 10 doesn't see; the
/// payer matches the selectors against the endpoint it picks when it prices a payment.
fn check_conversion_terms(
    asset: &Asset,
    conversion: &Conversion,
    accepted: &[&PaymentEndpointIdentifier],
) -> Result<()> {
    ensure(
        is_conversion_segment(asset.as_str()),
        "Paykit wire 4.4: a request with conversion terms is in an asset of lower-case letters and digits",
    )?;
    let Conversion::Fixed(rates) = conversion else {
        return Ok(());
    };
    for rate in rates {
        let selector: Vec<&str> = rate.asset().as_str().split('-').collect();
        ensure(
            (1..=2).contains(&selector.len()) && selector.iter().all(|s| is_conversion_segment(s)),
            "Paykit wire 4.4: a rate's asset is <asset> or <asset>-<rail>, each segment lower-case letters and digits",
        )?;
        let matches = |id: &&PaymentEndpointIdentifier| {
            let segments: Vec<&str> = id.as_str().split('-').collect();
            segments.get(..selector.len()) == Some(&selector[..])
        };
        ensure(
            accepted.is_empty() || accepted.iter().any(matches),
            "Paykit wire 4.4: a rate's asset matches no accepted endpoint",
        )?;
    }
    Ok(())
}

fn rates_json(rates: &[Rate]) -> Value {
    Value::Array(
        rates
            .iter()
            .map(|r| {
                let mut map = Map::new();
                map.insert("asset".into(), Value::String(r.asset().as_str().into()));
                map.insert("value".into(), Value::String(r.value().as_str().into()));
                Value::Object(map)
            })
            .collect(),
    )
}

fn read_label(obj: &Obj<'_>) -> Result<Option<Label>> {
    obj.opt_str("label")?
        .map(|l| {
            Label::parse(l)
                .map_err(|_| bad(obj.ctx(), "label", "isn't a valid label (Paykit wire 3.1)"))
        })
        .transpose()
}

fn insert_label(map: &mut Map<String, Value>, label: &Option<Label>) {
    if let Some(label) = label {
        map.insert("label".into(), Value::String(label.as_str().into()));
    }
}

fn time_value(ms: u64) -> Value {
    Value::String(rfc3339(ms))
}

fn unique_identifiers(
    value: &Value,
    ctx: &str,
    what: &str,
) -> Result<Vec<PaymentEndpointIdentifier>> {
    let items = value
        .as_array()
        .ok_or_else(|| bad(ctx, what, "not an array"))?;
    if items.is_empty() {
        return Err(bad(ctx, what, "empty"));
    }
    let mut out: Vec<PaymentEndpointIdentifier> = Vec::with_capacity(items.len());
    for item in items {
        let id = PaymentEndpointIdentifier::parse(
            item.as_str()
                .ok_or_else(|| bad(ctx, what, "not a string"))?,
        )?;
        if out.contains(&id) {
            return Err(bad(ctx, what, "an identifier twice"));
        }
        out.push(id);
    }
    Ok(out)
}

pub(crate) fn identifiers_json(ids: &[PaymentEndpointIdentifier]) -> Value {
    Value::Array(
        ids.iter()
            .map(|i| Value::String(i.as_str().into()))
            .collect(),
    )
}

/// A request's payment deadline (Paykit wire 4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaymentDeadline {
    /// `{ "type": "at", "timestamp": time }`, on a one-time request.
    At(u64),
    /// `{ "type": "period_start", "seconds": int }`, on a recurring request: the deadline is
    /// each period's start plus `seconds`.
    PeriodStart {
        /// Seconds after the period's start.
        seconds: u64,
    },
}

/// A request's conversion terms (Paykit wire 4.4, `specs/payment-conversion.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conversion {
    /// `{ "type": "fixed", "rates": rates }`.
    Fixed(Vec<Rate>),
    /// `{ "type": "per_period" }`, on a recurring request only: each period's rates come
    /// with its period details.
    PerPeriod,
}

/// Where a request is paid (Paykit spec 8.1, Paykit wire 4.5): a request carries at most one
/// of `payment_endpoints` and `required_app_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestDestination {
    /// `payment_endpoints`: the payer pays only to these, owned by the sending app.
    Endpoints(PaymentEndpoints),
    /// `required_app_id`: only to that payee app's Private Payment List or public endpoints.
    RequiredApp([u8; 16]),
    /// Neither: to any of the payee's current Private Payment Lists or public endpoints.
    Any,
}

/// The body of `paykit.payment_request` (Paykit wire 4.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequest {
    /// The amount requested.
    pub amount: PaymentAmount,
    /// The reference copied into the payment where the method allows it.
    pub payment_reference: PaymentReference,
    /// The proposal expiry, `None` for none; always written.
    pub proposal_expires_at: Option<u64>,
    /// The payment deadline, of the type matching the request.
    pub payment_deadline: Option<PaymentDeadline>,
    /// The recurrence of a recurring request; `None` for a one-time request.
    pub recurrence: Option<Recurrence>,
    /// Where the request is paid.
    pub destination: RequestDestination,
    /// With a recurrence: the request's endpoints serve only the first period, and later
    /// periods get theirs from period details.
    pub per_period_endpoints: bool,
    /// The labelled selling scope it is sent under.
    pub label: Option<Label>,
    /// The only identifiers the payer may use; with `payment_endpoints`, every key of that
    /// map is among them.
    pub accepted_payment_endpoint_identifiers: Option<Vec<PaymentEndpointIdentifier>>,
    /// The conversion terms.
    pub conversion: Option<Conversion>,
    /// Opaque metadata.
    pub metadata: Option<Map<String, Value>>,
}

const REQUEST: &str = "Paykit wire 4.5: payment_request";

impl PaymentRequest {
    /// Read a request's body (Paykit wire 4.5).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member or rule.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, REQUEST);
        let amount = read_amount(obj.req("amount")?, REQUEST, "amount")?;
        let payment_reference = PaymentReference::parse(obj.req_str("payment_reference")?)?;
        if !obj.has("proposal_expires_at") {
            return Err(bad(
                REQUEST,
                "proposal_expires_at",
                "missing; null for no expiry",
            ));
        }
        let proposal_expires_at = obj.opt_time("proposal_expires_at")?;
        let recurrence = obj
            .opt("recurrence")
            .map(Recurrence::from_json)
            .transpose()?;
        let payment_deadline = obj
            .opt_obj("payment_deadline")?
            .map(|d| match (d.req_str("type")?, recurrence.is_some()) {
                ("at", false) => Ok(PaymentDeadline::At(d.req_time("timestamp")?)),
                ("period_start", true) => Ok(PaymentDeadline::PeriodStart {
                    seconds: d.req_int("seconds")?,
                }),
                _ => Err(bad(
                    REQUEST,
                    "payment_deadline",
                    "its type doesn't match the request",
                )),
            })
            .transpose()?;
        let endpoints = obj
            .opt("payment_endpoints")
            .map(|e| read_endpoints(e, REQUEST))
            .transpose()?;
        let required_app = obj.opt_app_id("required_app_id")?;
        let destination = match (endpoints, required_app) {
            (Some(_), Some(_)) => {
                return invalid(
                    "Paykit wire 4.5: a request carries at most one of payment_endpoints and required_app_id",
                );
            }
            (Some(e), None) => RequestDestination::Endpoints(e),
            (None, Some(a)) => RequestDestination::RequiredApp(a),
            (None, None) => RequestDestination::Any,
        };
        let per_period_endpoints = obj.opt_bool("per_period_endpoints")?;
        if per_period_endpoints.is_some() && recurrence.is_none() {
            return Err(bad(REQUEST, "per_period_endpoints", "only with recurrence"));
        }
        let accepted = obj
            .opt("accepted_payment_endpoint_identifiers")
            .map(|a| unique_identifiers(a, REQUEST, "accepted_payment_endpoint_identifiers"))
            .transpose()?;
        if let (Some(ids), RequestDestination::Endpoints(e)) = (&accepted, &destination) {
            ensure(
                e.keys().all(|k| ids.contains(k)),
                "Paykit wire 4.5: every key of payment_endpoints is among accepted_payment_endpoint_identifiers",
            )?;
        }
        let conversion = obj
            .opt_obj("conversion")?
            .map(|c| match c.req_str("type")? {
                "fixed" => Ok(Conversion::Fixed(read_rates(c.req("rates")?, REQUEST)?)),
                "per_period" if recurrence.is_some() => Ok(Conversion::PerPeriod),
                _ => Err(bad(
                    REQUEST,
                    "conversion",
                    "type is fixed, or per_period on a recurring request",
                )),
            })
            .transpose()?;
        if let Some(conversion) = &conversion {
            // Paykit wire 1.4: requests with conversion terms use the recommended form.
            let mut ids: Vec<&PaymentEndpointIdentifier> = accepted.iter().flatten().collect();
            if let RequestDestination::Endpoints(e) = &destination {
                ids.extend(e.keys());
            }
            ensure(
                ids.iter().all(|i| i.is_recommended_form()),
                "Paykit wire 1.4: a request with conversion terms uses identifiers <asset>-<rail>-<format>",
            )?;
            check_conversion_terms(&amount.asset, conversion, &ids)?;
        }
        let metadata = obj
            .opt("metadata")
            .map(|m| {
                m.as_object()
                    .cloned()
                    .ok_or_else(|| bad(REQUEST, "metadata", "not an object"))
            })
            .transpose()?;
        Ok(Self {
            amount,
            payment_reference,
            proposal_expires_at,
            payment_deadline,
            recurrence,
            destination,
            per_period_endpoints: per_period_endpoints.unwrap_or(false),
            label: read_label(&obj)?,
            accepted_payment_endpoint_identifiers: accepted,
            conversion,
            metadata,
        })
    }

    /// Write the body: absent optional members omitted, `proposal_expires_at` always written,
    /// `per_period_endpoints` only when true.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        if let Some(ids) = &self.accepted_payment_endpoint_identifiers {
            map.insert(
                "accepted_payment_endpoint_identifiers".into(),
                identifiers_json(ids),
            );
        }
        map.insert("amount".into(), amount_json(&self.amount));
        if let Some(conversion) = &self.conversion {
            let mut c = Map::new();
            match conversion {
                Conversion::Fixed(rates) => {
                    c.insert("rates".into(), rates_json(rates));
                    c.insert("type".into(), Value::String("fixed".into()));
                }
                Conversion::PerPeriod => {
                    c.insert("type".into(), Value::String("per_period".into()));
                }
            }
            map.insert("conversion".into(), Value::Object(c));
        }
        insert_label(&mut map, &self.label);
        if let Some(metadata) = &self.metadata {
            map.insert("metadata".into(), Value::Object(metadata.clone()));
        }
        if let Some(deadline) = &self.payment_deadline {
            let mut d = Map::new();
            match deadline {
                PaymentDeadline::At(ms) => {
                    d.insert("timestamp".into(), time_value(*ms));
                    d.insert("type".into(), Value::String("at".into()));
                }
                PaymentDeadline::PeriodStart { seconds } => {
                    d.insert("seconds".into(), Value::from(*seconds));
                    d.insert("type".into(), Value::String("period_start".into()));
                }
            }
            map.insert("payment_deadline".into(), Value::Object(d));
        }
        match &self.destination {
            RequestDestination::Endpoints(e) => {
                map.insert("payment_endpoints".into(), endpoints_json(e));
            }
            RequestDestination::RequiredApp(app) => {
                map.insert("required_app_id".into(), Value::String(zbase32_encode(app)));
            }
            RequestDestination::Any => {}
        }
        map.insert(
            "payment_reference".into(),
            Value::String(self.payment_reference.as_str().into()),
        );
        if self.per_period_endpoints {
            map.insert("per_period_endpoints".into(), Value::Bool(true));
        }
        map.insert(
            "proposal_expires_at".into(),
            self.proposal_expires_at.map_or(Value::Null, time_value),
        );
        if let Some(recurrence) = &self.recurrence {
            map.insert("recurrence".into(), recurrence.to_json());
        }
        map
    }

    /// Whether the request is recurring.
    pub fn is_recurring(&self) -> bool {
        self.recurrence.is_some()
    }

    /// The Billing Period a message names, if it is one of this request's (Paykit wire 4.4).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a one-time request, or a named period that isn't one of the
    /// request's periods.
    pub fn named_period(&self, named: &PeriodRef) -> Result<BillingPeriod> {
        let recurrence = self.recurrence.as_ref().ok_or_else(|| {
            Error::Invalid("Paykit wire 4.4: a one-time request has no Billing Periods".into())
        })?;
        recurrence.named(named)?.ok_or_else(|| {
            Error::Invalid("Paykit wire 4.4: not a Billing Period of the request".into())
        })
    }

    /// The `deadline` an `attempt` carries (Paykit spec 7.1, Paykit wire 4.5): `d + 1 ms`,
    /// where for a one-time request `d` is the earlier of `proposal_expires_at` and the
    /// payment deadline's `timestamp`, and for a Billing Period of a recurring request the
    /// period's start plus the payment deadline's `seconds`. `None` when there is no `d`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a period with a one-time request, none with a recurring one, or
    /// a deadline past the last time the grammar writes.
    pub fn attempt_deadline(&self, period: Option<&BillingPeriod>) -> Result<Option<u64>> {
        let d = match (self.is_recurring(), period) {
            (false, None) => {
                let at = match self.payment_deadline {
                    Some(PaymentDeadline::At(ms)) => Some(ms),
                    _ => None,
                };
                match (self.proposal_expires_at, at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            }
            (true, Some(period)) => match self.payment_deadline {
                Some(PaymentDeadline::PeriodStart { seconds }) => Some(
                    seconds
                        .checked_mul(1000)
                        .and_then(|s| period.starts_at.checked_add(s))
                        .ok_or_else(|| {
                            Error::Invalid(
                                "Paykit wire 4.5: a deadline past the grammar's times".into(),
                            )
                        })?,
                ),
                _ => None,
            },
            (true, None) => {
                return invalid(
                    "Paykit wire 4.5: an attempt on a recurring request names a Billing Period",
                )
            }
            (false, Some(_)) => {
                return invalid("Paykit wire 4.5: a one-time request has no Billing Periods")
            }
        };
        Ok(d.map(attempt_deadline))
    }

    /// The `end` of the `server_time_in` condition that bounds the acceptance of a recurring
    /// request (Paykit spec 8.2, Paykit wire 4.5): `proposal_expires_at + 1 ms`, so the
    /// expiry's own millisecond is the last at which the acceptance is recorded. `None` for
    /// a request without an expiry.
    pub fn acceptance_end(&self) -> Option<u64> {
        self.proposal_expires_at.map(attempt_deadline)
    }
}

/// The body of `paykit.payment_request_acceptance` (Paykit wire 4.6): `{}`, or the labelled
/// request's `label`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaymentRequestAcceptance {
    /// The request's label, copied.
    pub label: Option<Label>,
}

impl PaymentRequestAcceptance {
    /// Read the body (Paykit wire 4.6).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a label that isn't valid.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        Ok(Self {
            label: read_label(&Obj::new(
                body,
                "Paykit wire 4.6: payment_request_acceptance",
            ))?,
        })
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        insert_label(&mut map, &self.label);
        map
    }
}

/// The body of `paykit.payment_request_rejection` and `paykit.payment_request_cancellation`
/// (Paykit wire 4.7, 4.8): an optional `reason`, and the label of a labelled request in a
/// payer's reply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestReply {
    /// The reason, if given.
    pub reason: Option<String>,
    /// The request's label, in a payer's reply to a labelled request; a payee's
    /// cancellation carries none.
    pub label: Option<Label>,
}

impl RequestReply {
    /// Read the body (Paykit wire 4.7, 4.8).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a `reason` that isn't a string or a label that isn't valid.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, "Paykit wire 4.7");
        Ok(Self {
            reason: obj.opt_str("reason")?.map(str::to_owned),
            label: read_label(&obj)?,
        })
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        insert_label(&mut map, &self.label);
        if let Some(reason) = &self.reason {
            map.insert("reason".into(), Value::String(reason.clone()));
        }
        map
    }
}

/// The body of `paykit.payment_period_details` (Paykit wire 4.9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeriodDetails {
    /// The Billing Period, one of the request's.
    pub billing_period: PeriodRef,
    /// The period's destination.
    pub payment_endpoints: Option<PaymentEndpoints>,
    /// The period's rates, cross-asset only.
    pub rates: Option<Vec<Rate>>,
    /// The payer uses only unexpired details; their rates apply to a payment verified at or
    /// before it.
    pub expires_at: u64,
}

const DETAILS: &str = "Paykit wire 4.9: payment_period_details";

impl PeriodDetails {
    /// Read the body (Paykit wire 4.9): at least one of `payment_endpoints` and `rates`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member or rule.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, DETAILS);
        let details = Self {
            billing_period: PeriodRef::from_json(obj.req("billing_period")?, DETAILS)?,
            payment_endpoints: obj
                .opt("payment_endpoints")
                .map(|e| read_endpoints(e, DETAILS))
                .transpose()?,
            rates: obj
                .opt("rates")
                .map(|r| read_rates(r, DETAILS))
                .transpose()?,
            expires_at: obj.req_time("expires_at")?,
        };
        ensure(
            details.payment_endpoints.is_some() || details.rates.is_some(),
            "Paykit wire 4.9: period details carry payment_endpoints, rates or both",
        )?;
        Ok(details)
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("billing_period".into(), self.billing_period.to_json());
        map.insert("expires_at".into(), time_value(self.expires_at));
        if let Some(e) = &self.payment_endpoints {
            map.insert("payment_endpoints".into(), endpoints_json(e));
        }
        if let Some(r) = &self.rates {
            map.insert("rates".into(), rates_json(r));
        }
        map
    }

    /// The checks that need the request (Paykit wire 4.9): the period is one of the
    /// request's, and the rates are cross-asset only, with no rate for the requested asset,
    /// bare (`usdt`) or rail-qualified (`usdt-polygon`), so a same-asset payment of a period
    /// stays 1:1 whatever its details say (`specs/payment-conversion.md`).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule.
    pub fn check_against(&self, request: &PaymentRequest) -> Result<BillingPeriod> {
        let period = request.named_period(&self.billing_period)?;
        let requested = request.amount.asset.as_str();
        if self.rates.iter().flatten().any(|r| {
            let selector = r.asset().as_str();
            selector == requested || selector_asset(selector) == requested
        }) {
            return invalid(
                "Paykit wire 4.9: period details' rates are cross-asset only, with no bare or rail-qualified rate for the requested asset",
            );
        }
        Ok(period)
    }
}

/// The order of period details for one period (Paykit wire 4.9): later means a later
/// `created_at` instant, then, at the same instant, the greater `msg_id` compared as text.
pub fn period_details_order(a: &Message, b: &Message) -> Ordering {
    a.created_at_ms
        .cmp(&b.created_at_ms)
        .then_with(|| a.msg_id.cmp(&b.msg_id))
}

/// The body of `paykit.payment_proof` (Paykit wire 4.10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentProof {
    /// The claim's attempt.
    pub attempt_id: AttemptId,
    /// The Billing Period paid, for a recurring request.
    pub billing_period: Option<PeriodRef>,
    /// The identifier paid.
    pub payment_endpoint_identifier: PaymentEndpointIdentifier,
    /// The payee app whose endpoint was paid.
    pub payment_app_id: [u8; 16],
    /// The Private Payment List `version`, when paid to one.
    pub list_version: Option<u64>,
    /// The amount, for a payment without a request; a request's comes from the request.
    pub amount: Option<PaymentAmount>,
    /// The Allowance of an automatic payment.
    pub allowance_id: Option<AllowanceId>,
    /// The labelled request's label.
    pub label: Option<Label>,
    /// The request's payment reference, if given.
    pub payment_reference: Option<PaymentReference>,
    /// The opaque, method-specific proof.
    pub proof: Map<String, Value>,
}

const PROOF: &str = "Paykit wire 4.10: payment_proof";

impl PaymentProof {
    /// Read the body (Paykit wire 4.10). A proof without `refs`, of a payment without a
    /// request, carries its `amount`; the caller passes how many `refs` the message has.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member or rule.
    pub fn from_body(body: &Map<String, Value>, refs: usize) -> Result<Self> {
        let obj = Obj::new(body, PROOF);
        let amount = obj
            .opt("amount")
            .map(|a| read_amount(a, PROOF, "amount"))
            .transpose()?;
        if refs == 0 && amount.is_none() {
            return Err(bad(
                PROOF,
                "amount",
                "a proof of a payment without a request carries it",
            ));
        }
        Ok(Self {
            attempt_id: AttemptId::from_bytes(obj.req_b64::<16>("attempt_id")?),
            billing_period: obj
                .opt("billing_period")
                .map(|p| PeriodRef::from_json(p, PROOF))
                .transpose()?,
            payment_endpoint_identifier: PaymentEndpointIdentifier::parse(
                obj.req_str("payment_endpoint_identifier")?,
            )?,
            payment_app_id: obj.req_app_id("payment_app_id")?,
            list_version: obj.opt_int("list_version")?,
            amount,
            allowance_id: obj
                .opt_str("allowance_id")?
                .map(AllowanceId::parse)
                .transpose()?,
            label: read_label(&obj)?,
            payment_reference: obj
                .opt_str("payment_reference")?
                .map(PaymentReference::parse)
                .transpose()?,
            proof: obj
                .req("proof")?
                .as_object()
                .cloned()
                .ok_or_else(|| bad(PROOF, "proof", "not an object"))?,
        })
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        if let Some(id) = &self.allowance_id {
            map.insert("allowance_id".into(), Value::String(id.as_str().into()));
        }
        if let Some(amount) = &self.amount {
            map.insert("amount".into(), amount_json(amount));
        }
        map.insert(
            "attempt_id".into(),
            Value::String(self.attempt_id.to_text()),
        );
        if let Some(p) = &self.billing_period {
            map.insert("billing_period".into(), p.to_json());
        }
        insert_label(&mut map, &self.label);
        if let Some(v) = self.list_version {
            map.insert("list_version".into(), Value::from(v));
        }
        map.insert(
            "payment_app_id".into(),
            Value::String(zbase32_encode(&self.payment_app_id)),
        );
        map.insert(
            "payment_endpoint_identifier".into(),
            Value::String(self.payment_endpoint_identifier.as_str().into()),
        );
        if let Some(r) = &self.payment_reference {
            map.insert("payment_reference".into(), Value::String(r.as_str().into()));
        }
        map.insert("proof".into(), Value::Object(self.proof.clone()));
        map
    }

    /// The settlement ID the proof names (Paykit wire 1.5), if its method gives one in its
    /// one spelling; a proof without one isn't credited as a payment without a request.
    pub fn settlement_id(&self) -> Option<Vec<u8>> {
        settlement_id(&self.proof)
    }

    /// The checks that need the request the proof's `refs` name (Paykit wire 4.10): a
    /// Billing Period exactly for a recurring request, one of its periods; the request's
    /// label copied; and a `payment_reference`, when given, equal to the request's.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule.
    pub fn check_against(&self, request: &PaymentRequest) -> Result<Option<BillingPeriod>> {
        ensure(
            self.label == request.label,
            "Paykit wire 4.10: a proof copies the request's label",
        )?;
        if let Some(reference) = &self.payment_reference {
            ensure(
                reference == &request.payment_reference,
                "Paykit wire 4.10: payment_reference equals the request's",
            )?;
        }
        match (&self.billing_period, request.is_recurring()) {
            (Some(named), true) => request.named_period(named).map(Some),
            (None, false) => Ok(None),
            _ => invalid(
                "Paykit wire 4.10: a proof names a Billing Period exactly for a recurring request",
            ),
        }
    }
}

/// The body of `paykit.payment_receipt` (Paykit wire 4.11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentReceipt {
    /// The attempt of the proof it confirms.
    pub attempt_id: AttemptId,
    /// The proof's Billing Period, for a recurring request.
    pub billing_period: Option<PeriodRef>,
}

const RECEIPT: &str = "Paykit wire 4.11: payment_receipt";

impl PaymentReceipt {
    /// Read the body (Paykit wire 4.11).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, RECEIPT);
        Ok(Self {
            attempt_id: AttemptId::from_bytes(obj.req_b64::<16>("attempt_id")?),
            billing_period: obj
                .opt("billing_period")
                .map(|p| PeriodRef::from_json(p, RECEIPT))
                .transpose()?,
        })
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert(
            "attempt_id".into(),
            Value::String(self.attempt_id.to_text()),
        );
        if let Some(p) = &self.billing_period {
            map.insert("billing_period".into(), p.to_json());
        }
        map
    }
}

/// The body of `paykit.resend_request` (Paykit wire 4.12): its selectors, which combine as a
/// union.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResendRequest {
    /// Messages by `msg_id`.
    pub msg_ids: Vec<String>,
    /// Messages the recipient's inbox accepted at or after this time.
    pub accepted_from: Option<u64>,
    /// ... and at or before this time; `None` for no upper bound.
    pub accepted_until: Option<u64>,
    /// The requests named, and every message whose `refs` name one of them.
    pub request_hashes: Vec<[u8; 32]>,
    /// With `request_hashes`: only messages whose body names that Billing Period.
    pub billing_period: Option<PeriodRef>,
}

const RESEND: &str = "Paykit wire 4.12: resend_request";

impl ResendRequest {
    /// Read the body (Paykit wire 4.12): an empty array counts as absent; at least one of
    /// `msg_ids`, `accepted_from` and `request_hashes` is present; `billing_period` is read
    /// only with `request_hashes`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member or rule.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, RESEND);
        let mut out = Self::default();
        for id in obj.opt_arr("msg_ids")?.into_iter().flatten() {
            let id = id
                .as_str()
                .ok_or_else(|| bad(RESEND, "msg_ids", "not a string"))?;
            pubky_switch_core::json::check_uuid_v4(id)
                .map_err(|_| bad(RESEND, "msg_ids", "not a lower-case UUID v4"))?;
            out.msg_ids.push(id.to_owned());
        }
        out.accepted_from = obj.opt_time("accepted_from")?;
        out.accepted_until = obj.opt_time("accepted_until")?;
        for hash in obj.opt_arr("request_hashes")?.into_iter().flatten() {
            let hash = hash
                .as_str()
                .ok_or_else(|| bad(RESEND, "request_hashes", "not a string"))?;
            out.request_hashes
                .push(crate::json::b64::<32>(hash, RESEND, "request_hashes")?);
        }
        if !out.request_hashes.is_empty() {
            out.billing_period = obj
                .opt("billing_period")
                .map(|p| PeriodRef::from_json(p, RESEND))
                .transpose()?;
        }
        ensure(
            !out.msg_ids.is_empty()
                || out.accepted_from.is_some()
                || !out.request_hashes.is_empty(),
            "Paykit wire 4.12: a resend_request selects something",
        )?;
        Ok(out)
    }

    /// Write the body: empty selectors left out, `billing_period` only with
    /// `request_hashes`.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        if let Some(t) = self.accepted_from {
            map.insert("accepted_from".into(), time_value(t));
        }
        if let Some(t) = self.accepted_until {
            map.insert("accepted_until".into(), time_value(t));
        }
        if !self.request_hashes.is_empty() {
            if let Some(p) = &self.billing_period {
                map.insert("billing_period".into(), p.to_json());
            }
            map.insert(
                "request_hashes".into(),
                Value::Array(
                    self.request_hashes
                        .iter()
                        .map(|h| Value::String(b64(h)))
                        .collect(),
                ),
            );
        }
        if !self.msg_ids.is_empty() {
            map.insert(
                "msg_ids".into(),
                Value::Array(
                    self.msg_ids
                        .iter()
                        .map(|i| Value::String(i.clone()))
                        .collect(),
                ),
            );
        }
        map
    }

    /// Whether the selectors select a message the sender sent (Paykit wire 4.12): by its
    /// `msg_id`; by any of the acceptance times the sender kept for it, both range ends
    /// inclusive at millisecond precision; or by a request hash, as that request (`hash`) or
    /// a message whose `refs` name it, narrowed with `billing_period` to messages whose body
    /// names that period (`period`), compared as instants.
    ///
    /// Which kinds the requester may get back is the caller's further filter (Paykit wire
    /// 4.12): kinds its certificate receives, and a payer's replies only under the label
    /// rule ([`crate::kinds::targets_app`]).
    pub fn selects(
        &self,
        message: &Message,
        hash: &[u8; 32],
        accepted_at: &[u64],
        period: Option<&PeriodRef>,
    ) -> bool {
        let by_id = self.msg_ids.contains(&message.msg_id);
        let by_time = self.accepted_from.is_some_and(|from| {
            accepted_at
                .iter()
                .any(|t| from <= *t && self.accepted_until.is_none_or(|until| *t <= until))
        });
        let named = self.request_hashes.contains(hash)
            || message.refs.iter().any(|r| self.request_hashes.contains(r));
        let by_request = named
            && match &self.billing_period {
                None => true,
                Some(p) => period.is_some_and(|q| q == p),
            };
        by_id || by_time || by_request
    }
}

/// One endpoint of a Private Payment List (Paykit wire 4.13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListEndpoint {
    /// The payload.
    pub payload: PaymentEndpointPayload,
    /// Whether paying it consumes this list version; written only when true.
    pub single_use: bool,
}

/// The body of `paykit.private_payment_list` (Paykit wire 4.13): the latest list of one
/// publishing app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivatePaymentList {
    /// Unix milliseconds of publishing, higher than every earlier version from this app.
    pub version: u64,
    /// The endpoints.
    pub payment_endpoints: BTreeMap<PaymentEndpointIdentifier, ListEndpoint>,
    /// When the list expires, chosen by the publishing app from its current time.
    pub expires_at: u64,
}

const LIST: &str = "Paykit wire 4.13: private_payment_list";

impl PrivatePaymentList {
    /// Read the body (Paykit wire 4.13). A list without `expires_at`, or with one that
    /// doesn't parse, is dropped.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, LIST);
        let mut endpoints = BTreeMap::new();
        for (id, endpoint) in obj
            .req("payment_endpoints")?
            .as_object()
            .ok_or_else(|| bad(LIST, "payment_endpoints", "not an object"))?
        {
            let e = Obj::of(endpoint, LIST, "payment_endpoints")?;
            endpoints.insert(
                PaymentEndpointIdentifier::parse(id)?,
                ListEndpoint {
                    payload: PaymentEndpointPayload::parse(e.req_str("payload")?)?,
                    single_use: e.opt_bool("single_use")?.unwrap_or(false),
                },
            );
        }
        Ok(Self {
            version: obj.req_int("version")?,
            payment_endpoints: endpoints,
            expires_at: obj.req_time("expires_at")?,
        })
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("expires_at".into(), time_value(self.expires_at));
        let endpoints = self
            .payment_endpoints
            .iter()
            .map(|(id, e)| {
                let mut m = Map::new();
                m.insert("payload".into(), Value::String(e.payload.as_str().into()));
                if e.single_use {
                    m.insert("single_use".into(), Value::Bool(true));
                }
                (id.as_str().to_owned(), Value::Object(m))
            })
            .collect();
        map.insert("payment_endpoints".into(), Value::Object(endpoints));
        map.insert("version".into(), Value::from(self.version));
        map
    }

    /// The instant from which a payer no longer uses the list (Paykit spec 5, Paykit wire
    /// 4.13): the earlier of `expires_at` and 30 days after its inbox accepted it.
    pub fn usable_until(&self, accepted_at: u64) -> u64 {
        self.expires_at
            .min(accepted_at.saturating_add(PRIVATE_LIST_MAX_AGE_MS))
    }
}

/// The body of `paykit.private_payment_list_request` (Paykit wire 4.14).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrivatePaymentListRequest {
    /// The `version` the sender holds, `None` when none; always written.
    pub held_version: Option<u64>,
    /// The publishing app asked about.
    pub app_id: Option<[u8; 16]>,
}

impl PrivatePaymentListRequest {
    /// Read the body (Paykit wire 4.14).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, "Paykit wire 4.14: private_payment_list_request");
        Ok(Self {
            held_version: obj.opt_int("held_version")?,
            app_id: obj.opt_app_id("app_id")?,
        })
    }

    /// Write the body, `held_version` always.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        if let Some(app) = &self.app_id {
            map.insert("app_id".into(), Value::String(zbase32_encode(app)));
        }
        map.insert(
            "held_version".into(),
            self.held_version.map_or(Value::Null, Value::from),
        );
        map
    }
}

/// The sender's role in an Allowance proposal (Paykit wire 4.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposerRole {
    /// The party whose apps pay.
    Allower,
    /// The party paid.
    Allowee,
}

/// The body of `paykit.allowance_proposal` (Paykit wire 4.15).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowanceProposal {
    /// The fresh Allowance ID.
    pub allowance_id: AllowanceId,
    /// The sender's role.
    pub proposer_role: ProposerRole,
    /// The terms, valid by Paykit wire 4.15.
    pub terms: AllowanceTerms,
}

const PROPOSAL: &str = "Paykit wire 4.15: allowance_proposal";

impl AllowanceProposal {
    /// Read the body (Paykit wire 4.15). Terms that aren't valid make the proposal one a
    /// recipient drops.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member or rule.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, PROPOSAL);
        let proposer_role = match obj.req_str("proposer_role")? {
            "allower" => ProposerRole::Allower,
            "allowee" => ProposerRole::Allowee,
            _ => return Err(bad(PROPOSAL, "proposer_role", "is allower or allowee")),
        };
        Ok(Self {
            allowance_id: AllowanceId::parse(obj.req_str("allowance_id")?)?,
            proposer_role,
            terms: AllowanceTerms::from_json(obj.req("terms")?)?,
        })
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert(
            "allowance_id".into(),
            Value::String(self.allowance_id.as_str().into()),
        );
        let role = match self.proposer_role {
            ProposerRole::Allower => "allower",
            ProposerRole::Allowee => "allowee",
        };
        map.insert("proposer_role".into(), Value::String(role.into()));
        map.insert("terms".into(), self.terms.to_json());
        map
    }
}

/// The body of `paykit.allowance_acceptance`, `paykit.allowance_rejection` and
/// `paykit.allowance_end` (Paykit wire 4.16).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowanceReply {
    /// The proposal's Allowance ID.
    pub allowance_id: AllowanceId,
    /// In an acceptance from the allower: the executor app at acceptance, for information.
    pub executor_app_id: Option<[u8; 16]>,
}

impl AllowanceReply {
    /// Read the body (Paykit wire 4.16).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, "Paykit wire 4.16");
        Ok(Self {
            allowance_id: AllowanceId::parse(obj.req_str("allowance_id")?)?,
            executor_app_id: obj.opt_app_id("executor_app_id")?,
        })
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert(
            "allowance_id".into(),
            Value::String(self.allowance_id.as_str().into()),
        );
        if let Some(app) = &self.executor_app_id {
            map.insert("executor_app_id".into(), Value::String(zbase32_encode(app)));
        }
        map
    }
}

/// Why an `executor_notice` is sent (Paykit wire 4.17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeReason {
    /// `ended`: the Allowance ended.
    Ended,
    /// `handed_off`: another app is the executor now.
    HandedOff,
}

/// The body of `paykit.executor_notice` (Paykit wire 4.17), between apps of one identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorNotice {
    /// Why.
    pub reason: NoticeReason,
    /// The record key of the Allowance record.
    pub allowance_record: [u8; 32],
}

impl ExecutorNotice {
    /// Read the body (Paykit wire 4.17).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an unknown reason or a record key that isn't 32 bytes.
    pub fn from_body(body: &Map<String, Value>) -> Result<Self> {
        let obj = Obj::new(body, "Paykit wire 4.17: executor_notice");
        let reason = match obj.req_str("reason")? {
            "ended" => NoticeReason::Ended,
            "handed_off" => NoticeReason::HandedOff,
            _ => return invalid("Paykit wire 4.17: reason is ended or handed_off"),
        };
        Ok(Self {
            reason,
            allowance_record: obj.req_b64::<32>("allowance_record")?,
        })
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert(
            "allowance_record".into(),
            Value::String(b64(&self.allowance_record)),
        );
        let reason = match self.reason {
            NoticeReason::Ended => "ended",
            NoticeReason::HandedOff => "handed_off",
        };
        map.insert("reason".into(), Value::String(reason.into()));
        map
    }
}

/// A Paykit message's body, read by its kind (Paykit wire 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// `paykit.payment_request`.
    PaymentRequest(PaymentRequest),
    /// `paykit.payment_request_acceptance`.
    PaymentRequestAcceptance(PaymentRequestAcceptance),
    /// `paykit.payment_request_rejection`.
    PaymentRequestRejection(RequestReply),
    /// `paykit.payment_request_cancellation`.
    PaymentRequestCancellation(RequestReply),
    /// `paykit.payment_period_details`.
    PaymentPeriodDetails(PeriodDetails),
    /// `paykit.payment_proof`.
    PaymentProof(PaymentProof),
    /// `paykit.payment_receipt`.
    PaymentReceipt(PaymentReceipt),
    /// `paykit.resend_request`.
    ResendRequest(ResendRequest),
    /// `paykit.private_payment_list`.
    PrivatePaymentList(PrivatePaymentList),
    /// `paykit.private_payment_list_request`.
    PrivatePaymentListRequest(PrivatePaymentListRequest),
    /// `paykit.allowance_proposal`.
    AllowanceProposal(AllowanceProposal),
    /// `paykit.allowance_acceptance`.
    AllowanceAcceptance(AllowanceReply),
    /// `paykit.allowance_rejection`.
    AllowanceRejection(AllowanceReply),
    /// `paykit.allowance_end`.
    AllowanceEnd(AllowanceReply),
    /// `paykit.executor_notice`.
    ExecutorNotice(ExecutorNotice),
}

impl Body {
    /// Read a Paykit message's body by its kind (check 10 of Paykit wire 4.1), and check the
    /// number of its `refs` (Paykit wire 4.3).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a kind that isn't Paykit's, an extension kind included, a
    /// `refs` count the kind doesn't allow, or a body that breaks its section's rules.
    pub fn parse(message: &Message) -> Result<Self> {
        let kind = Kind::from_kind(&message.kind).ok_or_else(|| {
            Error::Invalid("Paykit wire 4.1 check 10: a kind this reader doesn't handle".into())
        })?;
        if !kind.refs_counts().contains(&message.refs.len()) {
            return invalid("Paykit wire 4.3: refs aren't the kind's");
        }
        let body = &message.body;
        Ok(match kind {
            Kind::PaymentRequest => Self::PaymentRequest(PaymentRequest::from_body(body)?),
            Kind::PaymentRequestAcceptance => {
                Self::PaymentRequestAcceptance(PaymentRequestAcceptance::from_body(body)?)
            }
            Kind::PaymentRequestRejection => {
                Self::PaymentRequestRejection(RequestReply::from_body(body)?)
            }
            Kind::PaymentRequestCancellation => {
                Self::PaymentRequestCancellation(RequestReply::from_body(body)?)
            }
            Kind::PaymentPeriodDetails => {
                Self::PaymentPeriodDetails(PeriodDetails::from_body(body)?)
            }
            Kind::PaymentProof => {
                Self::PaymentProof(PaymentProof::from_body(body, message.refs.len())?)
            }
            Kind::PaymentReceipt => Self::PaymentReceipt(PaymentReceipt::from_body(body)?),
            Kind::ResendRequest => Self::ResendRequest(ResendRequest::from_body(body)?),
            Kind::PrivatePaymentList => {
                Self::PrivatePaymentList(PrivatePaymentList::from_body(body)?)
            }
            Kind::PrivatePaymentListRequest => {
                Self::PrivatePaymentListRequest(PrivatePaymentListRequest::from_body(body)?)
            }
            Kind::AllowanceProposal => Self::AllowanceProposal(AllowanceProposal::from_body(body)?),
            Kind::AllowanceAcceptance => {
                Self::AllowanceAcceptance(AllowanceReply::from_body(body)?)
            }
            Kind::AllowanceRejection => Self::AllowanceRejection(AllowanceReply::from_body(body)?),
            Kind::AllowanceEnd => Self::AllowanceEnd(AllowanceReply::from_body(body)?),
            Kind::ExecutorNotice => Self::ExecutorNotice(ExecutorNotice::from_body(body)?),
        })
    }

    /// The body's kind.
    pub fn kind(&self) -> Kind {
        match self {
            Self::PaymentRequest(_) => Kind::PaymentRequest,
            Self::PaymentRequestAcceptance(_) => Kind::PaymentRequestAcceptance,
            Self::PaymentRequestRejection(_) => Kind::PaymentRequestRejection,
            Self::PaymentRequestCancellation(_) => Kind::PaymentRequestCancellation,
            Self::PaymentPeriodDetails(_) => Kind::PaymentPeriodDetails,
            Self::PaymentProof(_) => Kind::PaymentProof,
            Self::PaymentReceipt(_) => Kind::PaymentReceipt,
            Self::ResendRequest(_) => Kind::ResendRequest,
            Self::PrivatePaymentList(_) => Kind::PrivatePaymentList,
            Self::PrivatePaymentListRequest(_) => Kind::PrivatePaymentListRequest,
            Self::AllowanceProposal(_) => Kind::AllowanceProposal,
            Self::AllowanceAcceptance(_) => Kind::AllowanceAcceptance,
            Self::AllowanceRejection(_) => Kind::AllowanceRejection,
            Self::AllowanceEnd(_) => Kind::AllowanceEnd,
            Self::ExecutorNotice(_) => Kind::ExecutorNotice,
        }
    }

    /// Write the body.
    pub fn to_body(&self) -> Map<String, Value> {
        match self {
            Self::PaymentRequest(b) => b.to_body(),
            Self::PaymentRequestAcceptance(b) => b.to_body(),
            Self::PaymentRequestRejection(b) | Self::PaymentRequestCancellation(b) => b.to_body(),
            Self::PaymentPeriodDetails(b) => b.to_body(),
            Self::PaymentProof(b) => b.to_body(),
            Self::PaymentReceipt(b) => b.to_body(),
            Self::ResendRequest(b) => b.to_body(),
            Self::PrivatePaymentList(b) => b.to_body(),
            Self::PrivatePaymentListRequest(b) => b.to_body(),
            Self::AllowanceProposal(b) => b.to_body(),
            Self::AllowanceAcceptance(b) | Self::AllowanceRejection(b) | Self::AllowanceEnd(b) => {
                b.to_body()
            }
            Self::ExecutorNotice(b) => b.to_body(),
        }
    }

    /// The `label` the body carries, for the request and the payer's replies.
    pub fn label(&self) -> Option<&Label> {
        match self {
            Self::PaymentRequest(b) => b.label.as_ref(),
            Self::PaymentRequestAcceptance(b) => b.label.as_ref(),
            Self::PaymentRequestRejection(b) | Self::PaymentRequestCancellation(b) => {
                b.label.as_ref()
            }
            Self::PaymentProof(b) => b.label.as_ref(),
            _ => None,
        }
    }

    /// The Billing Period the body names, for period details, proofs and receipts.
    pub fn billing_period(&self) -> Option<&PeriodRef> {
        match self {
            Self::PaymentPeriodDetails(b) => Some(&b.billing_period),
            Self::PaymentProof(b) => b.billing_period.as_ref(),
            Self::PaymentReceipt(b) => b.billing_period.as_ref(),
            _ => None,
        }
    }
}

/// Check 10 of a Paykit message (Paykit wire 4.1): the size of a kind that activity records
/// store (at most 16 KiB of JSON), then [`Body::parse`]. `json` is the message's exact JSON
/// bytes.
///
/// # Errors
///
/// As [`Body::parse`], and [`Error::Invalid`] for a recorded kind over 16 KiB.
pub fn check_body(message: &Message, json: &[u8]) -> Result<Body> {
    if Kind::from_kind(&message.kind).is_some_and(Kind::is_recorded_in_activity)
        && json.len() > MAX_RECORDED_KIND_LEN
    {
        return invalid("Paykit wire 4.1: a recorded kind is at most 16384 bytes of JSON");
    }
    Body::parse(message)
}

/// What check 10 hands on (Paykit wire 4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handled {
    /// A Paykit message, with its body.
    Paykit(Box<Body>),
    /// An extension kind the app handles: its body goes to the extension as it arrived.
    Extension(ExtensionKind),
}

/// Check 10 for an app that handles the extension kinds `handled` (Paykit wire 4.1, 4.3): an
/// extension kind it handles passes to the extension as it is; any other kind goes through
/// [`check_body`], which refuses every kind it doesn't handle.
///
/// # Errors
///
/// As [`check_body`].
pub fn check_body_handling(message: &Message, json: &[u8], handled: &[&str]) -> Result<Handled> {
    if handled.contains(&message.kind.as_str()) {
        if let Ok(MessageKind::Extension(ext)) = MessageKind::parse(&message.kind) {
            return Ok(Handled::Extension(ext));
        }
    }
    check_body(message, json).map(|body| Handled::Paykit(Box::new(body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_switch_core::message::Sender;
    use serde_json::json;

    fn body(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn request() -> Value {
        json!({ "amount": { "asset": "btc", "value": "0.00025" },
            "payment_deadline": { "timestamp": "2026-01-03T00:00:00.000Z", "type": "at" },
            "payment_endpoints": { "btc-lightning-bolt11": "lnbc1" },
            "payment_reference": "order-1042",
            "proposal_expires_at": "2026-01-02T00:00:00.000Z" })
    }

    fn message(kind: &str, value: Value, refs: usize) -> Message {
        Message {
            kind: kind.into(),
            msg_id: "1b4e28ba-2fa1-41d2-883f-0016d3cca427".into(),
            created_at_ms: 0,
            from: Sender {
                identity: [1; 32],
                app_id: [2; 16],
            },
            to: [3; 32],
            refs: vec![[9; 32]; refs],
            body: body(value),
        }
    }

    #[test]
    fn test_request_round_trips_and_gives_its_deadline() {
        let r = PaymentRequest::from_body(&body(request())).unwrap();
        assert_eq!(Value::Object(r.to_body()), request());
        let d = r.attempt_deadline(None).unwrap().unwrap();
        assert_eq!(
            d,
            pubky_switch_core::time::parse_rfc3339("2026-01-02T00:00:00.001Z").unwrap()
        );
        assert!(r
            .attempt_deadline(Some(&BillingPeriod {
                index: 0,
                starts_at: 0,
                ends_at: 1
            }))
            .is_err());
    }

    #[test]
    fn test_request_rejects_each_rule() {
        let cases: Vec<(&str, Value)> = vec![
            ("required_app_id", json!("brrz3m6g5rci6iqnzke44q4djc")),
            ("per_period_endpoints", json!(true)),
            ("conversion", json!({ "type": "per_period" })),
            (
                "payment_deadline",
                json!({ "type": "period_start", "seconds": 1 }),
            ),
            (
                "accepted_payment_endpoint_identifiers",
                json!(["other-x-y"]),
            ),
            ("accepted_payment_endpoint_identifiers", json!([])),
            ("label", json!("Shop1")),
            ("payment_reference", json!("")),
            ("metadata", json!("text")),
        ];
        for (member, value) in cases {
            let mut b = request();
            b[member] = value;
            assert!(PaymentRequest::from_body(&body(b)).is_err(), "{member}");
        }
        let mut no_expiry = request();
        no_expiry
            .as_object_mut()
            .unwrap()
            .remove("proposal_expires_at");
        assert!(PaymentRequest::from_body(&body(no_expiry)).is_err());
        let mut fixed = request();
        fixed["conversion"] =
            json!({ "type": "fixed", "rates": [{ "asset": "usd", "value": "1" }] });
        fixed["payment_endpoints"] = json!({ "LN": "lnbc1" });
        assert!(
            PaymentRequest::from_body(&body(fixed)).is_err(),
            "recommended form"
        );
    }

    #[test]
    fn test_request_recurring_deadline_and_acceptance_end() {
        let mut b = request();
        b["payment_deadline"] = json!({ "type": "period_start", "seconds": 86400 });
        b["recurrence"] = json!({ "anchor": "2026-01-31T00:00:00.000Z", "every": 1, "unit": "month",
            "starts_at": "2026-01-31T00:00:00.000Z", "ends_at": null });
        let r = PaymentRequest::from_body(&body(b)).unwrap();
        let period = r.recurrence.unwrap().first().unwrap();
        assert_eq!(
            r.attempt_deadline(Some(&period)).unwrap(),
            Some(period.starts_at + 86_400_001)
        );
        assert!(r.attempt_deadline(None).is_err());
        assert_eq!(r.acceptance_end(), r.proposal_expires_at.map(|e| e + 1));
    }

    #[test]
    fn test_period_details_need_endpoints_or_rates() {
        let ok = json!({ "billing_period": { "starts_at": "2026-02-28T00:00:00.000Z",
            "ends_at": "2026-03-31T00:00:00.000Z" }, "expires_at": "2026-03-01T00:00:00.000Z",
            "rates": [{ "asset": "btc", "value": "0.1" }] });
        let d = PeriodDetails::from_body(&body(ok.clone())).unwrap();
        assert_eq!(Value::Object(d.to_body()), ok);
        let mut neither = ok.clone();
        neither.as_object_mut().unwrap().remove("rates");
        assert!(PeriodDetails::from_body(&body(neither)).is_err());
        let mut twice = ok;
        twice["rates"] =
            json!([{ "asset": "btc", "value": "1" }, { "asset": "btc", "value": "2" }]);
        assert!(PeriodDetails::from_body(&body(twice)).is_err());
    }

    #[test]
    fn test_request_conversion_terms_constraints() {
        let fixed = |asset: &str, selectors: &[&str], accepted: Option<Value>| {
            let mut b = request();
            b["amount"] = json!({ "asset": asset, "value": "10" });
            b["payment_endpoints"] =
                json!({ "btc-lightning-bolt11": "lnbc1", "usdt-polygon-address": "0x1" });
            if let Some(ids) = accepted {
                b["accepted_payment_endpoint_identifiers"] = ids;
            }
            let rates: Vec<Value> = selectors
                .iter()
                .map(|s| json!({ "asset": s, "value": "1" }))
                .collect();
            b["conversion"] = json!({ "type": "fixed", "rates": rates });
            PaymentRequest::from_body(&body(b))
        };
        fixed("usd", &["btc", "usdt-polygon"], None).unwrap();
        fixed("usdt", &["usdt-polygon"], None).unwrap();
        let wider = json!([
            "btc-lightning-bolt11",
            "usdt-polygon-address",
            "eth-ethereum-address"
        ]);
        fixed("usd", &["eth"], Some(wider)).unwrap();
        for (asset, selectors, rule) in [
            ("USD", &["btc"][..], "lower-case letters and digits"),
            ("usd-x", &["btc"][..], "lower-case letters and digits"),
            ("usd", &["BTC"][..], "<asset>-<rail>"),
            ("usd", &["btc-Lightning"][..], "<asset>-<rail>"),
            ("usd", &["btc-lightning-bolt11"][..], "<asset>-<rail>"),
            ("usd", &["btc-"][..], "<asset>-<rail>"),
            ("usd", &["eth"][..], "matches no accepted endpoint"),
            (
                "usd",
                &["btc", "btc-onchain"][..],
                "matches no accepted endpoint",
            ),
            (
                "usd",
                &["usdt-ethereum"][..],
                "matches no accepted endpoint",
            ),
        ] {
            let error = fixed(asset, selectors, None).unwrap_err().to_string();
            assert!(error.contains(rule), "{asset} {selectors:?}: {error}");
        }
        // A per-period request in an asset outside the convention is refused too.
        let mut recurring = request();
        recurring["amount"] = json!({ "asset": "Usd", "value": "10" });
        recurring["payment_deadline"] = json!({ "type": "period_start", "seconds": 1 });
        recurring["recurrence"] = json!({ "anchor": "2026-01-31T00:00:00.000Z", "every": 1,
            "unit": "month", "starts_at": "2026-01-31T00:00:00.000Z", "ends_at": null });
        recurring["conversion"] = json!({ "type": "per_period" });
        assert!(PaymentRequest::from_body(&body(recurring)).is_err());
    }

    #[test]
    fn test_period_details_rates_are_cross_asset_only() {
        let mut recurring = request();
        recurring["amount"] = json!({ "asset": "usdt", "value": "10" });
        recurring["payment_deadline"] = json!({ "type": "period_start", "seconds": 86400 });
        recurring["recurrence"] = json!({ "anchor": "2026-01-31T00:00:00.000Z", "every": 1,
            "unit": "month", "starts_at": "2026-01-31T00:00:00.000Z", "ends_at": null });
        recurring["payment_endpoints"] =
            json!({ "btc-lightning-bolt11": "lnbc1", "usdt-polygon-address": "0x1" });
        recurring["conversion"] = json!({ "type": "per_period" });
        let request = PaymentRequest::from_body(&body(recurring)).unwrap();
        let details = |selector: &str| {
            PeriodDetails::from_body(&body(json!({ "billing_period": {
                "starts_at": "2026-01-31T00:00:00.000Z", "ends_at": "2026-02-28T00:00:00.000Z" },
                "expires_at": "2026-02-01T00:00:00.000Z",
                "rates": [{ "asset": selector, "value": "1.008" }] })))
            .unwrap()
        };
        details("btc").check_against(&request).unwrap();
        details("btc-lightning").check_against(&request).unwrap();
        details("usdtx").check_against(&request).unwrap();
        for same in ["usdt", "usdt-polygon", "usdt-ethereum"] {
            let error = details(same).check_against(&request).unwrap_err();
            assert!(error.to_string().contains("cross-asset only"), "{same}");
        }
    }

    #[test]
    fn test_proof_without_refs_carries_its_amount() {
        let p = json!({ "attempt_id": "mAFdg5_0tVNjmrgiTUZW4g", "payment_app_id": "brrz3m6g5rci6iqnzke44q4djc",
            "payment_endpoint_identifier": "btc-bitcoin-p2wpkh", "proof": { "type": "x" } });
        assert!(PaymentProof::from_body(&body(p.clone()), 1).is_ok());
        assert!(PaymentProof::from_body(&body(p.clone()), 0).is_err());
        let mut with = p;
        with["amount"] = json!({ "asset": "btc", "value": "0.1" });
        let proof = PaymentProof::from_body(&body(with.clone()), 0).unwrap();
        assert_eq!(Value::Object(proof.to_body()), with);
    }

    #[test]
    fn test_proof_check_against_its_request() {
        let r = PaymentRequest::from_body(&body(request())).unwrap();
        let p = json!({ "attempt_id": "mAFdg5_0tVNjmrgiTUZW4g", "payment_app_id": "brrz3m6g5rci6iqnzke44q4djc",
            "payment_endpoint_identifier": "btc-lightning-bolt11", "proof": { "type": "x" },
            "payment_reference": "order-1042" });
        let proof = PaymentProof::from_body(&body(p.clone()), 1).unwrap();
        assert_eq!(proof.check_against(&r).unwrap(), None);
        let mut other = p.clone();
        other["payment_reference"] = json!("order-1043");
        assert!(PaymentProof::from_body(&body(other), 1)
            .unwrap()
            .check_against(&r)
            .is_err());
        let mut labelled = p.clone();
        labelled["label"] = json!("shop1");
        assert!(PaymentProof::from_body(&body(labelled), 1)
            .unwrap()
            .check_against(&r)
            .is_err());
        let mut period = p;
        period["billing_period"] =
            json!({ "starts_at": "2026-01-01T00:00:00Z", "ends_at": "2026-01-02T00:00:00Z" });
        assert!(PaymentProof::from_body(&body(period), 1)
            .unwrap()
            .check_against(&r)
            .is_err());
    }

    #[test]
    fn test_resend_request_selectors() {
        for bad in [
            json!({}),
            json!({ "accepted_until": "2026-01-01T00:00:00.000Z" }),
            json!({ "msg_ids": [] }),
            json!({ "request_hashes": [] }),
            json!({ "billing_period": { "starts_at": "2026-01-01T00:00:00Z", "ends_at": "2026-01-02T00:00:00Z" } }),
        ] {
            assert!(
                ResendRequest::from_body(&body(bad.clone())).is_err(),
                "{bad}"
            );
        }
        let ids =
            json!({ "msg_ids": ["1b4e28ba-2fa1-41d2-883f-0016d3cca427"], "billing_period": "x" });
        let r = ResendRequest::from_body(&body(ids)).unwrap();
        assert!(r.billing_period.is_none());
        let m = message("paykit.payment_request", request(), 0);
        assert!(r.selects(&m, &[0; 32], &[], None));
        let by_time = ResendRequest {
            accepted_from: Some(10),
            accepted_until: Some(10),
            ..Default::default()
        };
        assert!(by_time.selects(&message("paykit.x", json!({}), 0), &[1; 32], &[10], None));
        assert!(!by_time.selects(&message("paykit.x", json!({}), 0), &[1; 32], &[11], None));
    }

    #[test]
    fn test_private_list_expiry_and_round_trip() {
        let l = json!({ "expires_at": "2026-01-31T00:06:00.000Z", "version": 1767225960000u64,
            "payment_endpoints": { "btc-bitcoin-p2wpkh": { "payload": "bc1q", "single_use": true },
                "btc-lightning-bolt12": { "payload": "lno1" } } });
        let list = PrivatePaymentList::from_body(&body(l.clone())).unwrap();
        assert_eq!(Value::Object(list.to_body()), l);
        assert_eq!(list.usable_until(0), PRIVATE_LIST_MAX_AGE_MS);
        assert_eq!(list.usable_until(list.expires_at), list.expires_at);
        let mut missing = l;
        missing.as_object_mut().unwrap().remove("expires_at");
        assert!(PrivatePaymentList::from_body(&body(missing)).is_err());
    }

    #[test]
    fn test_body_parse_checks_refs_and_kind() {
        assert!(Body::parse(&message("paykit.payment_request", request(), 0)).is_ok());
        assert!(Body::parse(&message("paykit.payment_request", request(), 1)).is_err());
        assert!(Body::parse(&message("paykit-cap.offer", json!({}), 0)).is_err());
        let ext = message("paykit-cap.offer", json!({ "a": 1 }), 0);
        assert!(matches!(
            check_body_handling(&ext, b"{}", &["paykit-cap.offer"]),
            Ok(Handled::Extension(_))
        ));
        assert!(check_body_handling(&ext, b"{}", &["paykit-cap.other"]).is_err());
        let big = vec![b' '; MAX_RECORDED_KIND_LEN + 1];
        assert!(check_body(&message("paykit.payment_request", request(), 0), &big).is_err());
    }

    #[test]
    fn test_executor_notice_reason() {
        let ok = json!({ "allowance_record": b64(&[1; 32]), "reason": "handed_off" });
        let n = ExecutorNotice::from_body(&body(ok.clone())).unwrap();
        assert_eq!(Value::Object(n.to_body()), ok);
        let mut bad = ok;
        bad["reason"] = json!("paused");
        assert!(ExecutorNotice::from_body(&body(bad)).is_err());
    }

    #[test]
    fn test_period_details_order_by_created_at_then_msg_id() {
        let mut a = message("paykit.payment_period_details", json!({}), 1);
        let mut b = a.clone();
        b.msg_id = "2b4e28ba-2fa1-41d2-883f-0016d3cca427".into();
        assert_eq!(period_details_order(&a, &b), Ordering::Less);
        a.created_at_ms = 1;
        assert_eq!(period_details_order(&a, &b), Ordering::Greater);
    }
}
