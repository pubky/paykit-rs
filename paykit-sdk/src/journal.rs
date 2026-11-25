//! The payment journal (Paykit spec 2, 7.2): one entry per payment, recorded before its
//! `attempt` is written, and moved through the paying order one committed stage at a time,
//! so that a crash at any point is resumed without a second payment.
//!
//! Crash rules (Paykit spec 7.2):
//!
//! - **Before paying** (`Recorded`, `Attempted`): no money moved. An own attempt still open is
//!   written `failed`.
//! - **Paying**: the payment may have been submitted. It is resolved from the payment
//!   method's history, never carried out again unless the method is idempotent on the
//!   attempt ID, the claim still shows the attempt open, and every check before a first
//!   handoff passes again; otherwise it stays open, "may be paid".
//! - **After paying** (`RailPaid`, `RailFailed`): the outcome, the activity record and the
//!   proof go in one batch, which also moves the entry to `Finished` once applied.
//! - **A restored journal** may predate a payment made after its backup, so its open attempts
//!   are resolved from a history that survives a restore, or left "may be paid" (Pubky Dispatch
//!   spec, Section 12.4).
//!
//! Sensitive: an entry holds the destination's payload, the message carrying it and the
//! rail's proof.

use paykit_lib::bodies::{Body, Conversion};
use paykit_lib::derive::{natural_key, payment_obligation_blinded, request_obligation_blinded};
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{DestinationSource, StoredMessage};
use paykit_lib::{
    Asset, AttemptId, Decimal, Label, ListVersion, PaymentAmount, PaymentEndpointIdentifier,
    PaymentEndpointPayload, PaymentReference, Rate,
};
use pubky_dispatch_core::chain::TransitionProof;
use pubky_dispatch_core::enc::{serde_b64, serde_b64_vec};
use pubky_dispatch_core::message::MessagePlaintext;
use pubky_dispatch_core::records::{AppCert, AppRecord};
use pubky_dispatch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::conversion::{PricedPayment, RateSource};
use crate::destination::Carrier;
use crate::error::{PaykitError, Result};
use crate::records::KeptMessage;
use crate::requests::RequestRef;

/// An optional byte string as base64url, or null.
mod serde_b64_opt {
    use pubky_dispatch_core::enc::{b64, unb64};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        value: &Option<Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(bytes) => serializer.serialize_some(&b64(bytes)),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| unb64(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// Where a payment stands in the paying order (Paykit spec 7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Stage {
    /// Recorded before the attempt write.
    Recorded,
    /// The attempt is written and its proof checked; no money moved.
    Attempted,
    /// Recorded just before the payment method's call: from here on it may be paid.
    Paying,
    /// The method reports it paid.
    RailPaid,
    /// The method reports a definitive failure.
    RailFailed,
    /// The outcome, the activity record and the proof were written in one batch, but the
    /// acceptance the payment owes isn't in the record yet: not queued, or queued
    /// (`acceptance_released`) and not yet placed. A resume queues it if it isn't, places it
    /// in the record, and finishes the entry (Paykit spec 7.2 steps 2, 3).
    AcceptanceOwed,
    /// Finished: the outcome, the activity record and, for a payment, the proof were written
    /// in one batch; or the failure was written before money could move.
    Finished,
    /// Given up before any money could move: the attempt wasn't written.
    Abandoned,
}

/// Where a payment goes, and where the destination came from (Paykit spec 7.2, Paykit wire
/// 5.3). Its `Debug` shows no payload.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Destination {
    #[serde(with = "serde_b64")]
    pub(crate) app_id: [u8; 16],
    pub(crate) endpoint_id: String,
    pub(crate) payload: String,
    pub(crate) source: String,
    pub(crate) list_version: Option<ListVersion>,
    pub(crate) single_use: bool,
}

impl std::fmt::Debug for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Destination({}, {})", self.endpoint_id, self.source)
    }
}

impl Destination {
    pub(crate) fn new(
        app_id: [u8; 16],
        endpoint_id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        source: DestinationSource,
        list_version: Option<ListVersion>,
        single_use: bool,
    ) -> Self {
        Self {
            app_id,
            endpoint_id: endpoint_id.as_str().to_owned(),
            payload: payload.as_str().to_owned(),
            source: match source {
                DestinationSource::Request => "request",
                DestinationSource::List => "list",
                DestinationSource::Public => "public",
            }
            .to_owned(),
            list_version,
            single_use,
        }
    }

    pub(crate) fn endpoint_id(&self) -> Result<PaymentEndpointIdentifier> {
        Ok(PaymentEndpointIdentifier::parse(&self.endpoint_id)?)
    }

    pub(crate) fn payload(&self) -> Result<PaymentEndpointPayload> {
        Ok(PaymentEndpointPayload::parse(&self.payload)?)
    }

    /// Where the destination came from; a kept value other than the three written is a
    /// corrupt store, never taken as one of them.
    pub(crate) fn source(&self) -> Result<DestinationSource> {
        match self.source.as_str() {
            "request" => Ok(DestinationSource::Request),
            "list" => Ok(DestinationSource::List),
            "public" => Ok(DestinationSource::Public),
            _ => Err(PaykitError::corrupt()),
        }
    }

    /// The destination's members read, and agree: a list version and a single-use mark only
    /// for a Private Payment List's endpoint.
    fn check(&self) -> Result<()> {
        self.endpoint_id()?;
        self.payload()?;
        let list = self.source()? == DestinationSource::List;
        if list != self.list_version.is_some() || (self.single_use && !list) {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }
}

/// The transfer an attempt may make (Paykit spec 7.2 step 1), as its planning allowed it:
/// the rule the final payment check holds the method's transfer to again, just before each
/// call, a repeat's included, so no call transfers more than the first was allowed to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TransferBound {
    /// What the user asked for: exactly the amount paid.
    Asked {},
    /// The selected transfer the user confirmed: its amount remains bound for every
    /// handoff.
    Confirmed {
        /// The amount confirmed; `None` for one the method couldn't tell.
        amount: Option<KeptAmount>,
    },
}

impl TransferBound {
    /// The exact amount this bound permits, excluding fees, or no amount when the user
    /// explicitly chose a transfer the payment method could not quantify.
    pub(crate) fn paid_amount(
        &self,
        payment_amount: &PaymentAmount,
    ) -> Result<Option<PaymentAmount>> {
        match self {
            Self::Asked { .. } => Ok(Some(payment_amount.clone())),
            Self::Confirmed { amount, .. } => amount.as_ref().map(KeptAmount::amount).transpose(),
        }
    }
}

/// The price of a payment priced by a rate, as its host authorized it (Paykit spec 7.2 step 1,
/// 8.2): the rate the call amount ([`JournalEntry::payment_amount`]) was computed by, with where the
/// rate comes from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct KeptPrice {
    /// The rate's asset selector, bare or rail-qualified.
    pub(crate) rate_asset: String,
    /// The rate's value, as a decimal.
    pub(crate) rate_value: String,
    /// Where the rate comes from.
    pub(crate) source: KeptRateSource,
}

/// Where a kept price's rate comes from ([`crate::RateSource`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum KeptRateSource {
    /// The request's fixed rates.
    Request,
    /// The rates of the period details with this `msg_id`.
    PeriodDetails {
        /// The period details' `msg_id`.
        msg_id: String,
        /// The signed rate source, kept for handoff, its proof and a later restore.
        details: Box<KeptMessage>,
    },
}

impl KeptPrice {
    /// The price of `shown`, as kept.
    pub(crate) fn of(shown: &PricedPayment, details: Option<&StoredMessage>) -> Self {
        let source = match details {
            Some(signed) => KeptRateSource::PeriodDetails {
                msg_id: signed.message().msg_id.clone(),
                details: Box::new(KeptMessage::of(signed)),
            },
            None => KeptRateSource::Request,
        };
        Self {
            rate_asset: shown.rate.asset().as_str().to_owned(),
            rate_value: shown.rate.value().as_str().to_owned(),
            source,
        }
    }

    /// The rate kept; a corrupt store for one that doesn't read.
    pub(crate) fn rate(&self) -> Result<Rate> {
        let asset = Asset::parse(&self.rate_asset).map_err(|_| PaykitError::corrupt())?;
        let value = Decimal::parse(&self.rate_value).map_err(|_| PaykitError::corrupt())?;
        Rate::new(asset, value).map_err(|_| PaykitError::corrupt())
    }

    /// Where the rate comes from.
    pub(crate) fn source(&self) -> RateSource {
        match &self.source {
            KeptRateSource::Request => RateSource::Request,
            KeptRateSource::PeriodDetails { msg_id, .. } => RateSource::PeriodDetails {
                msg_id: msg_id.clone(),
            },
        }
    }
}

/// An amount as kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptAmount {
    pub(crate) value: String,
    pub(crate) asset: String,
}

impl KeptAmount {
    pub(crate) fn of(amount: &PaymentAmount) -> Self {
        Self {
            value: amount.value.as_str().to_owned(),
            asset: amount.asset.as_str().to_owned(),
        }
    }

    pub(crate) fn amount(&self) -> Result<PaymentAmount> {
        Ok(PaymentAmount {
            value: Decimal::parse(&self.value)?,
            asset: Asset::parse(&self.asset)?,
        })
    }
}

/// A Billing Period as kept: its start and end, Unix milliseconds (Paykit wire 4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeptPeriod {
    pub(crate) starts_at: u64,
    pub(crate) ends_at: u64,
}

impl KeptPeriod {
    pub(crate) fn of(period: &PeriodRef) -> Self {
        Self {
            starts_at: period.starts_at,
            ends_at: period.ends_at,
        }
    }

    /// The period kept, which ends after it starts; one that doesn't is a corrupt store.
    pub(crate) fn period(self) -> Result<PeriodRef> {
        if self.ends_at <= self.starts_at {
            return Err(PaykitError::corrupt());
        }
        Ok(PeriodRef {
            starts_at: self.starts_at,
            ends_at: self.ends_at,
        })
    }
}

/// One payment in the journal (`p/j/<attempt_id>`). Sensitive.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct JournalEntry {
    /// The user started the original attempt. Its occurrence's manual-only mark keeps an
    /// automatic attempt from repeating, but permits this user's idempotent repeat (§8.3).
    pub(crate) user_started: bool,
    /// The original executor grant that authorized this attempt's reservation.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) allowance: Option<crate::executor::JournalAllowance>,
    /// The `expires_at` of the Allowance terms the reservation was made under, set with
    /// `allowance`: no first handoff goes once it has passed, while a repeat of a handoff
    /// that may have gone still reconciles (Paykit spec 8.3).
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) allowance_expires_at_ms: Option<u64>,
    pub(crate) attempt_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) obligation: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) payee: [u8; 32],
    /// The paid request, `None` for a payment without one.
    pub(crate) request_hash: Option<String>,
    /// The natural key of the payer's activity record.
    #[serde(with = "serde_b64_vec")]
    pub(crate) natural_key: Vec<u8>,
    pub(crate) amount: KeptAmount,
    /// The amount passed to the payment method: the request's computed amount or the
    /// amount of a payment without a request. TransferBound carries any actual override.
    pub(crate) payment_amount: KeptAmount,
    pub(crate) destination: Destination,
    pub(crate) deadline: Option<u64>,
    pub(crate) stage: Stage,
    /// The claim's step after the attempt, once written.
    pub(crate) step: Option<u64>,
    /// The request's label, which the payer's replies copy.
    pub(crate) label: Option<String>,
    pub(crate) payment_reference: Option<String>,
    /// The acceptance is owed with the payment (Paykit spec 7.2 step 2).
    pub(crate) acceptance_owed: bool,
    pub(crate) acceptance_msg_id: Option<String>,
    /// The acceptance's signed plaintext, for its stored copy in the request record, which
    /// the outcome's batch writes (Paykit spec 7.2 step 3).
    #[serde(with = "serde_b64_opt")]
    pub(crate) acceptance_plaintext: Option<Vec<u8>>,
    /// The app's own certificate the acceptance was signed under, kept with it: the
    /// acceptance is checked and placed under it, never under a later renewal, which may no
    /// longer hold `paykit_pay` (Paykit spec 3.2, Dispatch wire 3.8). Set and cleared with
    /// `acceptance_plaintext`.
    pub(crate) acceptance_cert: Option<SignedDoc>,
    /// The acceptance went into the outbox, sendable: only once the payment method's call
    /// was made, or a resume found the payment in the method's history (Paykit spec 7.2
    /// step 2). Until then it is journaled only, never sent and never placed in a record.
    pub(crate) acceptance_released: bool,
    /// The rail's proof, once paid.
    pub(crate) rail_proof: Option<Map<String, Value>>,
    pub(crate) paid_at_ms: Option<u64>,
    pub(crate) proof_msg_id: Option<String>,
    /// Recording a public payment made without the payer's Dispatch (Paykit spec 8.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) offline: Option<OfflineRecording>,
    /// The entry came back from a backup.
    pub(crate) restored: bool,
    /// Device time when journaled: the earliest a handoff could start.
    pub(crate) journaled_at_ms: u64,
    /// The verified message carrying the destination, for the destination check before a
    /// repeat (Paykit spec 7.2 step 1). Exactly one of it and `carrier_record` is kept: the
    /// carrier is journaled before the entry is first saved.
    pub(crate) carrier: Option<KeptMessage>,
    /// The signed app record carrying a public endpoint, for the same check.
    pub(crate) carrier_record: Option<SignedDoc>,
    /// The payment method reported the handoff failed, and `failed` was written on it: the
    /// method's history is asked again within its retention, and a payment that completed
    /// after all is reported with its proof (Paykit spec 7.2, "What a payment method
    /// declares").
    pub(crate) failed_by_method: bool,
    /// This device ended the Allowance attempt before entering the payment method.
    /// Restore clears the evidence because the original device may have continued.
    pub(crate) never_dispatched: bool,
    /// The user resolved this restored handoff as paid while no claim showed it, so its
    /// `done` went on no claim (Paykit spec 7.2, 8.4): no payment of its obligation goes
    /// again while it is journaled, as `done` on a claim would settle it.
    pub(crate) user_paid: bool,
    /// The attempt's transition proof, for the service and generation it was written at
    /// (Paykit spec 4, "Fresh before paying"). An entry without one is never handed off again.
    pub(crate) attempt_proof: Option<SignedDoc>,
    /// The request paid, as the payer's request record stored it: with the acceptance and
    /// the proof, what rebuilds that record should it be gone by the outcome, so `done`
    /// never goes without it (Paykit spec 7.2 step 3). Kept for every payment of a request,
    /// absent in a payment without a request.
    pub(crate) request: Option<KeptMessage>,
    /// The outcome of a payment made waits: the payer's activity record holds a value that
    /// doesn't read, which the SDK doesn't overwrite unasked, so neither `done` nor the
    /// proof is written. The attempt stays open, and nothing pays again. A durable notice
    /// for the host ([`PaymentView::record_held`]), until the record reads again or
    /// [`crate::Paykit::repair_activity_record`] writes it again.
    pub(crate) record_held: bool,
    /// The transfer the attempt may make, as its planning allowed it. `None` only in an
    /// entry a reconcile rebuilt from its claim (`restored`), which is never handed off
    /// again.
    pub(crate) transfer_bound: Option<TransferBound>,
    /// The price its user confirmed, for a payment priced by a rate: the rate `paid` was
    /// computed by, and `amount` stays the request's. `None` for a payment no rate prices,
    /// and in an entry a reconcile rebuilt, which is never handed off again.
    pub(crate) price: Option<KeptPrice>,
    /// The payment method reported the handoff failed, `failed` was written on the claim,
    /// and the method's history later showed it paid: its proof went, and `failed` stands on
    /// the claim until the payee's receipt settles it by evidence. A receipt that comes once
    /// the claim is gone raises [`crate::NoticeKind::PaidAfterFailed`] (Paykit spec 7.2,
    /// "What a payment method declares").
    pub(crate) failed_on_claim: bool,
    /// No payment started for this attempt (Paykit spec 7.2): this app wrote `failed` on the
    /// claim while it was still before the payment method's call, or the method refused its
    /// first call, which says nothing was and nothing will be submitted. Set in the journal
    /// copy the failed outcome's own batch carries, so the entry holds it however that batch
    /// came to apply, and on the `RailFailed` entry a refusal leaves.
    pub(crate) failed_before_call: bool,
    /// How the user resolved this attempt ([`crate::Resolution`]), on its claim or, for a
    /// restored handoff no claim shows, off it (Paykit spec 7.3). Set in the journal copy the
    /// resolution's batch carries, so the entry holds it however that batch came to apply.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) user_resolved: Option<UserResolution>,
    /// The Billing Period paid, for a recurring request (Paykit spec 8.2): the obligation is
    /// the period's, the record the outcome writes is the payer's period record, and the
    /// proof names the period. `None` for a one-time request and a payment without one.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) billing_period: Option<KeptPeriod>,
}

/// The user's resolution of an attempt, as its journal entry keeps it
/// ([`crate::Resolution`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UserResolution {
    /// The user confirmed it was paid.
    Done,
    /// The user couldn't tell.
    Unknown,
    /// The user confirmed it wasn't paid.
    Failed,
}

/// The members of a journal entry whose values only go together as the journal's writers
/// leave them ([`members_hold`]).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EntryMembers {
    pub(crate) stage: Option<Stage>,
    pub(crate) proof: bool,
    pub(crate) failed_by_method: bool,
    pub(crate) failed_before_call: bool,
    pub(crate) failed_on_claim: bool,
    pub(crate) record_held: bool,
    pub(crate) user_resolved: Option<UserResolution>,
    pub(crate) user_paid: bool,
    pub(crate) restored: bool,
}

/// Whether an entry holds these members together, as the journal's writers leave them
/// ([`JournalEntry::check`]). Anything else is a corrupt store.
///
/// 1. The method's proof only at `RailPaid`, `AcceptanceOwed` or `Finished`, and always at
///    the first two: it is journaled with `RailPaid`, and the outcome moves it on.
/// 2. The method's failure only at `RailFailed` or `Finished`, never beside its proof: the
///    recheck clears it before it journals a proof.
/// 3. "No payment started" ([`JournalEntry::failed_before_call`]) only at `RailFailed`, as a
///    refused first call leaves it, or `Finished`, as this app's own `failed` before the
///    method's call does; never with a proof or the method's failure.
/// 4. A failure on the claim only beside a proof; a record held only beside a proof, at
///    `RailPaid`, where the outcome waits for it.
/// 5. The user's resolution, on the claim ([`JournalEntry::user_resolved`]) or off it
///    ([`JournalEntry::user_paid`]), only at `Finished`, as the resolution finishes the entry
///    (Paykit spec 7.3), or beside the method's proof. One
///    writer leaves it there: the recheck of a payment the method reported failed, which
///    opens the entry again at `RailPaid` with the proof once the method's history shows the
///    payment made after all, the user's resolution kept beside it (Paykit spec 7.2, "What a
///    payment method declares"). Never on an open entry without a proof, which would finish
///    reported resolved over what the method said.
/// 6. The user's `done` off the claim ([`JournalEntry::user_paid`]) only on a restored
///    entry, and never beside the user's resolution as failed or unknown: the same
///    resolution writes it as done. Without a resolution beside it, which no writer leaves,
///    it is held all the same: it can only withhold a payment, never make one.
pub(crate) fn members_hold(m: &EntryMembers) -> bool {
    let Some(stage) = m.stage else {
        return false;
    };
    let proof_placed = if m.proof {
        matches!(
            stage,
            Stage::RailPaid | Stage::AcceptanceOwed | Stage::Finished
        )
    } else {
        !matches!(stage, Stage::RailPaid | Stage::AcceptanceOwed)
    };
    let failure_placed =
        !m.failed_by_method || matches!(stage, Stage::RailFailed | Stage::Finished) && !m.proof;
    let no_payment_started_placed = !m.failed_before_call
        || matches!(stage, Stage::RailFailed | Stage::Finished) && !m.proof && !m.failed_by_method;
    let claim_failure_placed = !m.failed_on_claim || m.proof;
    let record_held_placed = !m.record_held || m.proof && stage == Stage::RailPaid;
    let resolution_placed =
        m.user_resolved.is_none() && !m.user_paid || stage == Stage::Finished || m.proof;
    let user_paid_placed =
        !m.user_paid || m.restored && matches!(m.user_resolved, None | Some(UserResolution::Done));
    proof_placed
        && failure_placed
        && no_payment_started_placed
        && claim_failure_placed
        && record_held_placed
        && resolution_placed
        && user_paid_placed
}

impl std::fmt::Debug for JournalEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JournalEntry({:?})", self.stage)
    }
}

/// An offline payment starts without a shared attempt. A restored copy of any state
/// may predate a submission by the original device (Paykit spec 8.4).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum OfflineRecording {
    AwaitingRecording,
    Applied,
}

impl JournalEntry {
    /// Keep the original device's evidence when ending a reservation (Paykit spec 8.3).
    pub(crate) fn end_before_the_call(&mut self) {
        if self.allowance.is_some()
            && !self.restored
            && matches!(self.stage, Stage::Recorded | Stage::Attempted)
        {
            self.never_dispatched = true;
        }
    }

    /// The amount passed to the payment method, computed and bound before handoff.
    pub(crate) fn call_amount(&self) -> Result<PaymentAmount> {
        self.payment_amount.amount()
    }

    /// Exact transfer evidence bound before handoff, excluding fees. A method whose
    /// transfer was unknown and an outcome-only reconstruction supply no numeric amount.
    pub(crate) fn paid_amount(&self) -> Result<Option<PaymentAmount>> {
        self.transfer_bound
            .as_ref()
            .map(|bound| bound.paid_amount(&self.call_amount()?))
            .transpose()
            .map(Option::flatten)
    }

    /// The signed rates carried by this period's selected details (Paykit wire 4.9).
    /// Verify the occurrence, its payee, and the rate for the exact paid endpoint.
    pub(crate) fn selected_rate_details(&self, own: &[u8; 32]) -> Result<Option<StoredMessage>> {
        let Some(price) = &self.price else {
            return Ok(None);
        };
        let KeptRateSource::PeriodDetails { msg_id, details } = &price.source else {
            return Ok(None);
        };
        let signed = details.stored(own)?;
        let request = self
            .request
            .as_ref()
            .ok_or_else(PaykitError::corrupt)?
            .stored(own)?;
        let (Body::PaymentRequest(parent), Body::PaymentPeriodDetails(body)) =
            (request.body(), signed.body())
        else {
            return Err(PaykitError::corrupt());
        };
        let endpoint = self.destination.endpoint_id()?;
        let rates = body.rates.as_deref().ok_or_else(PaykitError::corrupt)?;
        let rate =
            paykit_lib::rate_for_endpoint(rates, &endpoint).map_err(|_| PaykitError::corrupt())?;
        if request.sender() != self.payee
            || Some(request.hash()) != self.request_hash()?
            || signed.sender() != self.payee
            || &signed.message().msg_id != msg_id
            || signed.message().refs != [request.hash()]
            || Some(body.billing_period) != self.period()?
            || body.check_against(parent).is_err()
            || signed.check_payee_sender(parent).is_err()
            || rate != Some(&price.rate()?)
        {
            return Err(PaykitError::corrupt());
        }
        Ok(Some(signed))
    }

    /// The single details message named by this occurrence's proof. A later
    /// destination and any per-period rate must come from that same signed message.
    pub(crate) fn selected_period_details(&self, own: &[u8; 32]) -> Result<Option<StoredMessage>> {
        let rates = self.selected_rate_details(own)?;
        let carrier = self.kept_carrier(own)?;
        let destination = match carrier {
            Carrier::Message(message)
                if matches!(message.body(), Body::PaymentPeriodDetails(_)) =>
            {
                Some(*message)
            }
            _ => None,
        };
        let Some(period) = self.period()? else {
            return if destination.is_none() {
                Ok(rates)
            } else {
                Err(PaykitError::corrupt())
            };
        };
        let request = self
            .request
            .as_ref()
            .ok_or_else(PaykitError::corrupt)?
            .stored(own)?;
        let Body::PaymentRequest(parent) = request.body() else {
            return Err(PaykitError::corrupt());
        };
        let first = parent
            .recurrence
            .as_ref()
            .ok_or_else(PaykitError::corrupt)?
            .first()?;
        if !parent.per_period_endpoints || period == PeriodRef::from(first) {
            return if destination.is_none() {
                Ok(rates)
            } else {
                Err(PaykitError::corrupt())
            };
        }
        let signed = destination.ok_or_else(PaykitError::corrupt)?;
        let Body::PaymentPeriodDetails(details) = signed.body() else {
            return Err(PaykitError::corrupt());
        };
        let endpoint = self.destination.endpoint_id()?;
        if Some(request.hash()) != self.request_hash()?
            || request.sender() != self.payee
            || signed.sender() != self.payee
            || signed.message().refs != [request.hash()]
            || signed.cert().app_id() != self.destination.app_id
            || details.billing_period != period
            || details.check_against(parent).is_err()
            || signed.check_payee_sender(parent).is_err()
            || self.destination.source()? != DestinationSource::Request
            || !crate::destination::period_endpoint_matches(details, &self.destination)
            || matches!(&parent.destination, paykit_lib::bodies::RequestDestination::RequiredApp(app) if *app != self.destination.app_id)
            || parent
                .accepted_payment_endpoint_identifiers
                .as_ref()
                .is_some_and(|ids| !ids.contains(&endpoint))
            || rates
                .as_ref()
                .is_some_and(|source| source.hash() != signed.hash())
        {
            return Err(PaykitError::corrupt());
        }
        Ok(Some(signed))
    }

    /// The originally authorized quote from the kept attempt and verified rate details.
    pub(crate) fn priced_payment(
        &self,
        selected: Option<&StoredMessage>,
    ) -> Result<Option<PricedPayment>> {
        let Some(price) = &self.price else {
            return Ok(None);
        };
        let Some(request_hash) = self.request_hash()? else {
            return Err(PaykitError::corrupt());
        };
        let payload = self.destination.payload()?;
        Ok(Some(PricedPayment {
            request: RequestRef {
                payee: self.payee,
                request_hash,
            },
            billing_period: self.period()?,
            rate_source_hash: selected.map_or(request_hash, StoredMessage::hash),
            app_id: self.destination.app_id,
            endpoint_id: self.destination.endpoint_id()?,
            payload_hash: pubky_dispatch_core::enc::hash(payload.as_str().as_bytes()),
            requested: self.amount.amount()?,
            source: price.source(),
            rate: price.rate()?,
            amount: self.payment_amount.amount()?,
        }))
    }

    pub(crate) fn attempt(&self) -> Result<AttemptId> {
        AttemptId::parse(&self.attempt_id).map_err(|_| PaykitError::corrupt())
    }

    pub(crate) fn request_hash(&self) -> Result<Option<[u8; 32]>> {
        self.request_hash
            .as_deref()
            .map(|h| {
                pubky_dispatch_core::enc::unb64_array::<32>(h).map_err(|_| PaykitError::corrupt())
            })
            .transpose()
    }

    /// The Billing Period paid, read as kept: `None` for a payment of no period.
    pub(crate) fn period(&self) -> Result<Option<PeriodRef>> {
        self.billing_period.map(KeptPeriod::period).transpose()
    }

    /// The start of the Billing Period paid in Unix seconds, as obligation IDs name it
    /// (Paykit wire 1.5); 0 for a one-time request.
    pub(crate) fn period_start_secs(&self) -> Result<u64> {
        Ok(self.period()?.map_or(0, |period| period.start_secs()))
    }

    /// The natural key of the payer's request record of the request paid, which the attempt
    /// is conditioned on and a cancellation stops (Paykit spec 7.2, 8.2): the record the
    /// outcome writes for a one-time request, the request record beside the period record
    /// for a Billing Period. `None` for a payment without a request.
    pub(crate) fn request_record(&self) -> Result<Option<Vec<u8>>> {
        Ok(self
            .request_hash()?
            .map(|hash| natural_key::request_record(&self.payee, &hash)))
    }

    /// The label kept, read as written: one that doesn't read is a corrupt store.
    pub(crate) fn label(&self) -> Result<Option<Label>> {
        self.label
            .as_deref()
            .map(Label::parse)
            .transpose()
            .map_err(|_| PaykitError::corrupt())
    }

    /// Check an entry read back from the store or a backup: every member reads, and they
    /// agree (Paykit spec 7.2). A malformed entry is refused as a corrupt store, never taken
    /// with a member defaulted: a damaged label or destination would send a payment's
    /// evidence where it doesn't belong.
    ///
    /// - the attempt ID, the request hash, the amount, the label and the payment reference
    ///   read; the destination's members read and agree;
    /// - a payment of a request has its payment reference, and its record's natural key is
    ///   the request's, or, for a Billing Period, the period's, which owes no acceptance; a
    ///   payment without a request has neither, nor a period, and owes no acceptance (its
    ///   claim key, blinded, is checked by [`JournalEntry::check_bound`]);
    /// - an acceptance is kept with its plaintext, which reads, and only when owed;
    /// - the method's proof is kept from `RailPaid` on, and before any payment time or
    ///   proof message; a `Finished` entry marked failed by the method holds no proof;
    /// - at most one carrier, which reads; the attempt's proof, when kept, reads;
    /// - a kept request only in a payment of a request, and it reads; an outcome held for
    ///   its record, and a payment made after `failed` was written, only once paid by the
    ///   method.
    pub(crate) fn check(&self) -> Result<()> {
        let bad = || Err(PaykitError::corrupt());
        self.attempt()?;
        let request_hash = self.request_hash()?;
        self.amount.amount()?;
        self.destination.check()?;
        self.label()?;
        let period = self.period()?;
        // An expiry is the reserving Allowance's, kept only with it.
        if self.allowance.is_none() && self.allowance_expires_at_ms.is_some() {
            return bad();
        }
        if self.offline.is_some()
            && (request_hash.is_some()
                || self.destination.source()? != DestinationSource::Public
                || self.destination.single_use
                || self.transfer_bound.is_none()
                || self.carrier_record.is_none())
        {
            return bad();
        }
        match (&request_hash, &self.payment_reference) {
            (Some(hash), Some(reference)) => {
                PaymentReference::parse(reference).map_err(|_| PaykitError::corrupt())?;
                let natural = match &period {
                    // A subscription is accepted before its periods are paid (Paykit spec
                    // 8.2): no acceptance goes with a period's payment.
                    Some(_) if self.acceptance_owed => return bad(),
                    Some(period) => {
                        natural_key::period_record(&self.payee, hash, period.start_secs())
                    }
                    None => natural_key::request_record(&self.payee, hash),
                };
                if self.natural_key != natural {
                    return bad();
                }
            }
            (None, None) => {
                if self.acceptance_owed
                    || period.is_some()
                    || self.destination.source()? == DestinationSource::Request
                {
                    return bad();
                }
            }
            _ => return bad(),
        }
        if self.acceptance_msg_id.is_some() != self.acceptance_plaintext.is_some()
            || (self.acceptance_msg_id.is_some() && !self.acceptance_owed)
            || (self.acceptance_released && self.acceptance_msg_id.is_none())
        {
            return bad();
        }
        if let Some(bytes) = &self.acceptance_plaintext {
            MessagePlaintext::decode(bytes).map_err(|_| PaykitError::corrupt())?;
        }
        // The acceptance and the certificate it was signed under are kept, and cleared,
        // together.
        if self.acceptance_cert.is_some() != self.acceptance_plaintext.is_some() {
            return bad();
        }
        let paid_members = self.paid_at_ms.is_some() || self.proof_msg_id.is_some();
        if paid_members && self.rail_proof.is_none() {
            return bad();
        }
        if self.never_dispatched
            && (self.allowance.is_none()
                || self.restored
                || !matches!(self.stage, Stage::Finished | Stage::Abandoned)
                || self.acceptance_released
                || self.failed_by_method
                || self.rail_proof.is_some())
        {
            return bad();
        }
        if !members_hold(&EntryMembers {
            stage: Some(self.stage),
            proof: self.rail_proof.is_some(),
            failed_by_method: self.failed_by_method,
            failed_before_call: self.failed_before_call,
            failed_on_claim: self.failed_on_claim,
            record_held: self.record_held,
            user_resolved: self.user_resolved,
            user_paid: self.user_paid,
            restored: self.restored,
        }) {
            return bad();
        }
        // A payment of a request keeps the request, and only such a payment does.
        match (&self.request, request_hash) {
            (Some(kept), Some(_)) => kept.check_shape()?,
            (None, None) => {}
            _ => return bad(),
        }
        // Only a reconcile's rebuilt entry, never handed off again, has no transfer bound.
        if self.transfer_bound.is_none()
            && (!self.restored
                || !matches!(
                    self.stage,
                    Stage::RailPaid | Stage::RailFailed | Stage::AcceptanceOwed | Stage::Finished
                ))
        {
            return bad();
        }
        // The call amount is the amount asked, but for a payment of a request: one priced by
        // a rate, planned with a transfer bound, to an endpoint of the paid asset that the
        // rate prices; another, the amount asked rounded up, in its asset, to an endpoint of
        // that asset.
        let asked = self.amount.amount()?;
        let paid = self.payment_amount.amount()?;
        if self.price.is_none()
            && paid != asked
            && (request_hash.is_none()
                || paid.asset != asked.asset
                || paid.value < asked.value
                || self.destination.endpoint_id()?.asset_segment() != Some(paid.asset.as_str()))
        {
            return bad();
        }
        if let Some(price) = &self.price {
            let rate = price.rate()?;
            let endpoint_id = self.destination.endpoint_id()?;
            if request_hash.is_none()
                || self.paid_amount()?.as_ref() != Some(&paid)
                || self.transfer_bound.is_none()
                || endpoint_id.asset_segment() != Some(paid.asset.as_str())
                || !matches!(
                    paykit_lib::rate_for_endpoint(std::slice::from_ref(&rate), &endpoint_id),
                    Ok(Some(_))
                )
            {
                return bad();
            }
        }
        // Owed: an acceptance to queue or to place, beside the proof ([`members_hold`]). One queued and not yet
        // placed is released and still owed here, as `finish_owed_acceptance` saves it
        // between the queueing and the record's batch.
        if self.stage == Stage::AcceptanceOwed && !self.acceptance_owed {
            return bad();
        }
        match (&self.carrier, &self.carrier_record) {
            (Some(_), Some(_)) => return bad(),
            (Some(kept), None) => kept.check_shape()?,
            (None, Some(signed)) => {
                AppRecord::verify(signed, &self.payee).map_err(|_| PaykitError::corrupt())?;
            }
            (None, None) => return bad(),
        }
        if let Some(signed) = &self.attempt_proof {
            TransitionProof::peek(signed).map_err(|_| PaykitError::corrupt())?;
        }
        Ok(())
    }

    /// `entry`, read at its attempt's key, bound to `obligation`, the claim its caller looks
    /// the attempt up for: an entry of the attempt on another claim would decide this claim
    /// from another payment's state. A corrupt store, never an absent entry.
    pub(crate) fn on_claim(entry: Option<Self>, obligation: &[u8; 32]) -> Result<Option<Self>> {
        match entry {
            Some(entry) if entry.obligation != *obligation => Err(PaykitError::corrupt()),
            entry => Ok(entry),
        }
    }

    /// [`JournalEntry::check`], and the entry's claim key bound to what it pays, each input
    /// blinded by `blind` with the payer's naming key (Paykit spec 7.1, Paykit wire 1.5): for
    /// a payment of a request, the obligation of the payee, the request and the start of the
    /// Billing Period paid, in Unix seconds, 0 for a one-time request; for a payment without
    /// a request, the obligation of its attempt ID. An entry whose claim key names another
    /// obligation would read and write one claim while its record and proof name another: it
    /// is a corrupt store.
    pub(crate) fn check_bound(&self, blind: impl Fn(&[u8]) -> [u8; 32]) -> Result<()> {
        self.check()?;
        let expected = match self.request_hash()? {
            Some(hash) => request_obligation_blinded(
                &blind(&self.payee),
                &blind(&hash),
                self.period_start_secs()?,
            ),
            None => payment_obligation_blinded(&blind(&self.attempt()?.to_bytes())),
        };
        if self.obligation != expected {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }

    /// The entry as a restore takes it (Paykit spec 7.2), before anything is written:
    /// [`JournalEntry::check_bound`], and every message the entry keeps verified again for
    /// `own`, as its later use verifies it, so a restore never installs what that use would
    /// refuse.
    ///
    /// - The request another party signed: its certificate, signature and recipient, its
    ///   sender the payee, its hash the request hash the entry pays, a `payment_request`,
    ///   and its amount, payment reference and label the entry's.
    /// - The Billing Period paid: for a recurring request, one of the request's own Billing
    ///   Periods, its start and end as the request's recurrence computes them, and the
    ///   entry's deadline that period's attempt deadline; a one-time request's entry names
    ///   no period.
    /// - The destination's carrier: the list message or app record, verified again.
    /// - The acceptance this app signed: read as its own message to the payee, under the
    ///   certificate it was signed under ([`JournalEntry::acceptance_message`]), an
    ///   acceptance of that request with the `msg_id` the entry keeps.
    ///
    /// A corrupt store for any mismatch.
    pub(crate) fn check_restored(
        &self,
        own: &[u8; 32],
        own_cert: &AppCert,
        blind: impl Fn(&[u8]) -> [u8; 32],
    ) -> Result<()> {
        self.check_bound(blind)?;
        let corrupt = || Err(PaykitError::corrupt());
        let request_hash = self.request_hash()?;
        if let Some(kept) = &self.request {
            let Some(hash) = request_hash else {
                return corrupt();
            };
            let request = kept.stored(own)?;
            let Body::PaymentRequest(body) = request.body() else {
                return corrupt();
            };
            let reference = self
                .payment_reference
                .as_deref()
                .map(PaymentReference::parse)
                .transpose()
                .map_err(|_| PaykitError::corrupt())?;
            if request.hash() != hash
                || request.sender() != self.payee
                || reference.as_ref() != Some(&body.payment_reference)
                || self.amount.amount()? != body.amount
                || self.label()? != body.label
            {
                return corrupt();
            }
            // A payment of a recurring request pays one of its Billing Periods, the attempt
            // carrying that period's deadline; a payment of a one-time request pays none
            // (Paykit spec 7.1, 8.2; Paykit wire 4.4).
            match self.period()? {
                Some(period) => {
                    let Ok(billing) = body.named_period(&period) else {
                        return corrupt();
                    };
                    let deadline = body
                        .attempt_deadline(Some(&billing))
                        .map_err(|_| PaykitError::corrupt())?;
                    if self.deadline != deadline {
                        return corrupt();
                    }
                }
                None if body.is_recurring() => return corrupt(),
                None => {}
            }
            // The signed conversion terms determine whether a rate is required; omitting
            // a kept price cannot turn a priced endpoint into an unpriced one (Paykit spec 7.2).
            let endpoint_id = self.destination.endpoint_id()?;
            match (&body.conversion, &self.price) {
                (_, None) if self.transfer_bound.is_none() => {}
                (Some(Conversion::Fixed(rates)), price) => {
                    let Ok(expected) = paykit_lib::rate_for_endpoint(rates, &endpoint_id) else {
                        return corrupt();
                    };
                    match (expected, price) {
                        (Some(expected), Some(price))
                            if matches!(price.source, KeptRateSource::Request)
                                && price.rate()? == *expected => {}
                        (None, None)
                            if endpoint_id.asset_segment() == Some(body.amount.asset.as_str()) => {}
                        _ => return corrupt(),
                    }
                }
                (None, None) => {}
                (Some(Conversion::PerPeriod), Some(price))
                    if matches!(price.source, KeptRateSource::PeriodDetails { .. }) => {}
                (Some(Conversion::PerPeriod), None)
                    if endpoint_id.asset_segment() == Some(body.amount.asset.as_str()) => {}
                _ => return corrupt(),
            }
        }
        self.selected_period_details(own)?;
        if let Some(acceptance) = self
            .acceptance_message(own, own_cert)
            .map_err(|_| PaykitError::corrupt())?
        {
            let Some(hash) = request_hash else {
                return corrupt();
            };
            if !matches!(acceptance.body(), Body::PaymentRequestAcceptance(_))
                || acceptance.message().refs != [hash]
                || self.acceptance_msg_id.as_deref() != Some(acceptance.message().msg_id.as_str())
            {
                return corrupt();
            }
        }
        Ok(())
    }

    /// The acceptance this app signed, as its own message to the payee, under the certificate
    /// it was signed under ([`JournalEntry::acceptance_cert`]); `current` is this app's
    /// certificate now, whose App ID the acceptance must carry. It is read as any reader reads
    /// a stored message
    /// ([`StoredMessage::new`], Paykit wire 5.2: the certificate verified for this identity,
    /// its App ID the plaintext's, the signature, and the sender's scopes); a kept
    /// certificate must also be this app's own. So a later renewal, revocation of a scope or
    /// key-preserving change never refuses an acceptance its certificate authorized, and one
    /// no certificate of this app authorized is still refused. `None` when the entry keeps
    /// no acceptance.
    pub(crate) fn acceptance_message(
        &self,
        own: &[u8; 32],
        current: &AppCert,
    ) -> Result<Option<StoredMessage>> {
        let Some(bytes) = &self.acceptance_plaintext else {
            return Ok(None);
        };
        let plaintext = MessagePlaintext::decode(bytes).map_err(|_| PaykitError::corrupt())?;
        let cert = self
            .acceptance_cert
            .as_ref()
            .ok_or_else(PaykitError::corrupt)?;
        let stored = StoredMessage::new(*own, cert, plaintext, None, &self.payee)?;
        // Not another own app's acceptance: the plaintext names the certificate's App ID,
        // which must be this app's.
        if stored.message().from.app_id != current.app_id() {
            return Err(PaykitError::corrupt());
        }
        Ok(Some(stored))
    }

    /// Keep the destination's carrier in the entry.
    pub(crate) fn keep_carrier(&mut self, carrier: &Carrier) {
        match carrier {
            Carrier::Message(message) => self.carrier = Some(KeptMessage::of(message)),
            Carrier::Record(record) => self.carrier_record = Some(record.signed().clone()),
        }
    }

    /// The destination's carrier, verified again for `own`. An entry that keeps none is a
    /// corrupt store ([`JournalEntry::check`] refuses it).
    pub(crate) fn kept_carrier(&self, own: &[u8; 32]) -> Result<Carrier> {
        if let Some(kept) = &self.carrier {
            return Ok(Carrier::Message(Box::new(kept.stored(own)?)));
        }
        match &self.carrier_record {
            Some(signed) => AppRecord::verify(signed, &self.payee)
                .map(|record| Carrier::Record(Box::new(record)))
                .map_err(|_| PaykitError::corrupt()),
            None => Err(PaykitError::corrupt()),
        }
    }

    /// Whether the entry still has work: not finished, not abandoned.
    pub(crate) fn open(&self) -> bool {
        !matches!(
            self.stage,
            Stage::AcceptanceOwed | Stage::Finished | Stage::Abandoned
        )
    }

    /// Whether the payment method's call may have been made for this entry ([`handed_off`]).
    pub(crate) fn may_have_handed_off(&self) -> bool {
        handed_off(self.stage, self.restored)
    }

    /// The request's acceptance is this entry's to send and place: owed, queued or not yet
    /// (Paykit spec 7.2 step 2). Placing a released one again is a no-op.
    pub(crate) fn may_place_acceptance(&self) -> Result<bool> {
        Ok(self.acceptance_owed && self.request_hash()?.is_some())
    }

    /// Whether the entry owes an acceptance it hasn't released yet: a one-time request's,
    /// for a payment made.
    pub(crate) fn owes_acceptance(&self) -> Result<bool> {
        Ok(self.acceptance_owed && !self.acceptance_released && self.request_hash()?.is_some())
    }
}

/// A payment in the journal, as the host sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PaymentView {
    /// The attempt ID, the payment method's idempotency key.
    pub attempt_id: AttemptId,
    /// The claim key of the obligation paid.
    pub obligation: [u8; 32],
    /// The payee.
    pub payee: [u8; 32],
    /// The request paid, `None` for a payment without one.
    pub request_hash: Option<[u8; 32]>,
    /// The Billing Period paid, for a recurring request (Paykit spec 8.2); `None` for a
    /// one-time request and a payment without one.
    pub billing_period: Option<paykit_lib::periods::PeriodRef>,
    /// The amount.
    pub amount: PaymentAmount,
    /// The identifier paid.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// Where it stands.
    pub stage: Stage,
    /// The entry came back from a backup.
    pub restored: bool,
    /// Whether the payment method reported this payment made: the journal holds its proof.
    /// At `Finished` it tells a payment made, its outcome written, from one written failed.
    /// After a call returned a wallet-wide stop, it is how the host learns of a payment made
    /// in that call, as the call's error carries no outcome (Paykit spec 7.2).
    pub paid_by_method: bool,
    /// Paid by the method, its outcome waits: the payer's activity record holds a value that
    /// doesn't read, so `done` and the proof aren't written, and the attempt stays open
    /// (Paykit spec 7.2 step 3). It settles once the record reads again, at
    /// [`crate::Paykit::resume_payments`], or when the user has the record written again
    /// with [`crate::Paykit::repair_activity_record`].
    pub record_held: bool,
    /// The exact transferred amount bound before handoff, excluding fees. `None` when
    /// the method could not quantify the selected transfer or the entry was rebuilt only
    /// to report an outcome. The requested amount is never substituted for missing evidence.
    pub paid: Option<PaymentAmount>,
    /// The rate used for a payment priced by a rate; `None` for another payment.
    pub rate: Option<Rate>,
    /// The payee's receipt of this attempt the payer's activity record holds, if any
    /// (Paykit wire 4.11), as this app's copy shows it: for display. While the records
    /// aren't listed, after a restore or a certificate that reads more, the copy may lack a
    /// receipt the Dispatch holds; [`crate::Paykit::receipts`] is refused as not now
    /// meanwhile.
    pub receipt: Option<crate::ReceivedReceipt>,
}

impl JournalEntry {
    pub(crate) fn view(&self) -> Result<PaymentView> {
        Ok(PaymentView {
            attempt_id: self.attempt()?,
            obligation: self.obligation,
            payee: self.payee,
            request_hash: self.request_hash()?,
            billing_period: self.period()?,
            amount: self.amount.amount()?,
            paid: self.paid_amount()?,
            rate: self.price.as_ref().map(KeptPrice::rate).transpose()?,
            endpoint_id: self.destination.endpoint_id()?,
            stage: self.stage,
            restored: self.restored,
            paid_by_method: self.rail_proof.is_some(),
            record_held: self.record_held,
            receipt: None,
        })
    }
}

/// Whether the payment method's call may have been made, as the acceptance's release reads
/// it (Paykit spec 7.2 step 2): at `Paying`, journaled just before the call, or for a
/// restored entry not yet past it, whose handoff the backup can't tell. Before `Paying` the
/// call is known not made; at `RailPaid` the method's history spoke (its paid branch releases
/// the acceptance), and at `RailFailed` it reported a definitive failure, so a later user's
/// `done` releases none.
pub(crate) fn handed_off(stage: Stage, restored: bool) -> bool {
    match stage {
        Stage::Paying => true,
        Stage::Recorded | Stage::Attempted => restored,
        Stage::RailPaid
        | Stage::RailFailed
        | Stage::AcceptanceOwed
        | Stage::Finished
        | Stage::Abandoned => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_journal_members_hold_only_as_their_writers_leave_them() {
        use Stage::{
            Abandoned, AcceptanceOwed, Attempted, Finished, Paying, RailFailed, RailPaid, Recorded,
        };
        use UserResolution::{Done, Failed, Unknown};
        let at = |stage| EntryMembers {
            stage: Some(stage),
            ..EntryMembers::default()
        };
        let paid = |stage| EntryMembers {
            proof: true,
            ..at(stage)
        };
        let method_failed = |stage| EntryMembers {
            failed_by_method: true,
            ..at(stage)
        };
        let no_payment_started = |stage| EntryMembers {
            failed_before_call: true,
            ..at(stage)
        };
        let resolved = |m: EntryMembers, resolution| EntryMembers {
            user_resolved: Some(resolution),
            ..m
        };
        let paid_off_claim = |m: EntryMembers, resolution| EntryMembers {
            user_paid: true,
            restored: true,
            user_resolved: resolution,
            ..m
        };
        let rows: Vec<(&str, EntryMembers, bool)> = vec![
            // 1. The proof.
            ("an entry before the method's call", at(Recorded), true),
            ("an entry paying", at(Paying), true),
            ("a proof journaled", paid(RailPaid), true),
            (
                "a proof with its acceptance owed",
                paid(AcceptanceOwed),
                true,
            ),
            ("a proof finished", paid(Finished), true),
            ("RailPaid without a proof", at(RailPaid), false),
            (
                "an acceptance owed without a proof",
                at(AcceptanceOwed),
                false,
            ),
            ("a proof before the call", paid(Attempted), false),
            ("a proof while paying", paid(Paying), false),
            ("a proof failed", paid(RailFailed), false),
            ("a proof abandoned", paid(Abandoned), false),
            // 2. The method's failure.
            (
                "the method's failure journaled",
                method_failed(RailFailed),
                true,
            ),
            (
                "the method's failure finished",
                method_failed(Finished),
                true,
            ),
            (
                "the method's failure while paying",
                method_failed(Paying),
                false,
            ),
            (
                "the method's failure beside a proof",
                EntryMembers {
                    proof: true,
                    ..method_failed(Finished)
                },
                false,
            ),
            // 3. No payment started.
            ("a refused first call", no_payment_started(RailFailed), true),
            ("this app's own failure", no_payment_started(Finished), true),
            (
                "no payment started before the call",
                no_payment_started(Attempted),
                false,
            ),
            (
                "no payment started beside a proof",
                EntryMembers {
                    proof: true,
                    ..no_payment_started(Finished)
                },
                false,
            ),
            (
                "no payment started beside the method's failure",
                EntryMembers {
                    failed_by_method: true,
                    ..no_payment_started(Finished)
                },
                false,
            ),
            // 4. The claim's failure and a held record.
            (
                "a failure on the claim beside a proof the recheck found",
                EntryMembers {
                    failed_on_claim: true,
                    ..paid(RailPaid)
                },
                true,
            ),
            (
                "a failure on the claim without a proof",
                EntryMembers {
                    failed_on_claim: true,
                    ..at(Finished)
                },
                false,
            ),
            (
                "a record held for a paid outcome",
                EntryMembers {
                    record_held: true,
                    ..paid(RailPaid)
                },
                true,
            ),
            (
                "a record held once finished",
                EntryMembers {
                    record_held: true,
                    ..paid(Finished)
                },
                false,
            ),
            (
                "a record held without a proof",
                EntryMembers {
                    record_held: true,
                    ..at(RailFailed)
                },
                false,
            ),
            // 5. The user's resolution.
            ("the user's done", resolved(at(Finished), Done), true),
            ("the user's unknown", resolved(at(Finished), Unknown), true),
            ("the user's failed", resolved(at(Finished), Failed), true),
            (
                "the user's done over the method's failure",
                resolved(method_failed(Finished), Done),
                true,
            ),
            (
                "the user's failed over a refused call",
                resolved(no_payment_started(Finished), Failed),
                true,
            ),
            (
                "the user's failed beside a proof the recheck found",
                resolved(paid(RailPaid), Failed),
                true,
            ),
            (
                "the user's done open beside the method's failure",
                resolved(method_failed(RailFailed), Done),
                false,
            ),
            (
                "the user's failed while paying",
                resolved(at(Paying), Failed),
                false,
            ),
            // 6. The user's done off the claim.
            (
                "the user's done off the claim",
                paid_off_claim(at(Finished), Some(Done)),
                true,
            ),
            (
                "the user's done off the claim beside a proof the recheck found",
                paid_off_claim(paid(RailPaid), Some(Done)),
                true,
            ),
            (
                "paid off the claim and resolved failed",
                paid_off_claim(at(Finished), Some(Failed)),
                false,
            ),
            (
                "paid off the claim and resolved unknown",
                paid_off_claim(at(Finished), Some(Unknown)),
                false,
            ),
            // No writer leaves it, and it only withholds a payment.
            (
                "paid off the claim with no resolution",
                paid_off_claim(at(Finished), None),
                true,
            ),
            (
                "paid off the claim on an open entry without a proof",
                paid_off_claim(at(RailFailed), None),
                false,
            ),
            (
                "paid off the claim, not restored",
                EntryMembers {
                    restored: false,
                    ..paid_off_claim(at(Finished), Some(Done))
                },
                false,
            ),
        ];
        for (what, members, holds) in rows {
            assert_eq!(members_hold(&members), holds, "{what}: {members:?}");
        }
    }

    #[test]
    fn test_journal_transfer_bound_refuses_a_fee_from_earlier_builds() {
        let read = |v: serde_json::Value| serde_json::from_value::<TransferBound>(v);
        assert_eq!(
            read(serde_json::json!({ "kind": "asked" })).unwrap(),
            TransferBound::Asked {}
        );
        assert!(read(serde_json::json!({ "kind": "confirmed", "amount": null })).is_ok());
        // A cap or a fee is no member of the bound: an entry holding one is refused.
        for earlier in [
            serde_json::json!({ "kind": "asked", "cap": null }),
            serde_json::json!({ "kind": "confirmed", "cap": "1", "amount": null, "max_fee": null }),
        ] {
            assert!(read(earlier).is_err());
        }
    }

    #[test]
    fn test_journal_handed_off_only_at_paying_or_restored_before_an_outcome() {
        let stages = [
            (Stage::Recorded, false, true),
            (Stage::Attempted, false, true),
            (Stage::Paying, true, true),
            (Stage::RailPaid, false, false),
            (Stage::RailFailed, false, false),
            (Stage::AcceptanceOwed, false, false),
            (Stage::Finished, false, false),
            (Stage::Abandoned, false, false),
        ];
        for (stage, fresh, restored) in stages {
            assert_eq!(handed_off(stage, false), fresh, "{stage:?}");
            assert_eq!(handed_off(stage, true), restored, "{stage:?} restored");
        }
    }

    #[test]
    fn test_journal_destination_debug_hides_the_payload() {
        let destination = Destination::new(
            [1; 16],
            &PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            &PaymentEndpointPayload::new("lnbc-secret").unwrap(),
            DestinationSource::Request,
            None,
            false,
        );
        let text = format!("{destination:?}");
        assert!(!text.contains("secret"), "{text}");
        assert_eq!(destination.source().unwrap(), DestinationSource::Request);
        // A source other than the three written isn't taken as one of them.
        let mut unknown = destination.clone();
        unknown.source = "elsewhere".into();
        assert!(unknown.source().is_err());
        assert!(unknown.check().is_err());
        assert_eq!(destination.payload().unwrap().as_str(), "lnbc-secret");
    }

    #[test]
    fn test_journal_amount_round_trips_and_refuses_garbage() {
        let amount = PaymentAmount {
            value: Decimal::new("0.00025").unwrap(),
            asset: Asset::new("btc").unwrap(),
        };
        assert_eq!(KeptAmount::of(&amount).amount().unwrap(), amount);
        let bad = KeptAmount {
            value: "-1".into(),
            asset: "btc".into(),
        };
        assert!(bad.amount().is_err());
    }
}
