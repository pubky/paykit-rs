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
//!   dispatch passes again; otherwise it stays open, "may be paid".
//! - **After paying** (`RailPaid`, `RailFailed`): the outcome, the activity record and the
//!   proof go in one batch, which also moves the entry to `Finished` once applied.
//! - **A restored journal** may predate a payment made after its backup, so its open attempts
//!   are resolved from a history that survives a restore, or left "may be paid" (Pubky Switch
//!   spec, Section 12.4).
//!
//! Sensitive: an entry holds the destination's payload, the message carrying it and the
//! rail's proof.

use paykit_lib::bodies::Body;
use paykit_lib::derive::{natural_key, payment_obligation};
use paykit_lib::records::{DestinationSource, StoredMessage};
use paykit_lib::{
    Asset, AttemptId, Decimal, Label, PaymentAmount, PaymentEndpointIdentifier,
    PaymentEndpointPayload, PaymentReference,
};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::enc::{serde_b64, serde_b64_vec};
use pubky_switch_core::message::MessagePlaintext;
use pubky_switch_core::records::{AppCert, AppRecord};
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::destination::Carrier;
use crate::error::{PaykitError, Result};
use crate::records::KeptMessage;

/// An optional byte string as base64url, or null.
mod serde_b64_opt {
    use pubky_switch_core::enc::{b64, unb64};
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
    pub(crate) list_version: Option<u64>,
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
        list_version: Option<u64>,
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

/// One payment in the journal (`p/j/<attempt_id>`). Sensitive.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct JournalEntry {
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
    #[serde(default, with = "serde_b64_opt")]
    pub(crate) acceptance_plaintext: Option<Vec<u8>>,
    /// The app's own certificate the acceptance was signed under, kept with it: the
    /// acceptance is checked and placed under it, never under a later renewal, which may no
    /// longer hold `pay` (Paykit spec 3.2, Switch wire 3.8). `None` for an entry written
    /// before it was kept, checked under the current certificate as then.
    #[serde(default)]
    pub(crate) acceptance_cert: Option<SignedDoc>,
    /// The acceptance went into the outbox, sendable: only once the payment method's call
    /// was made, or a resume found the payment in the method's history (Paykit spec 7.2
    /// step 2). Until then it is journaled only, never sent and never placed in a record.
    #[serde(default)]
    pub(crate) acceptance_released: bool,
    /// The rail's proof, once paid.
    pub(crate) rail_proof: Option<Map<String, Value>>,
    pub(crate) paid_at_ms: Option<u64>,
    pub(crate) proof_msg_id: Option<String>,
    /// The entry came back from a backup.
    pub(crate) restored: bool,
    /// Device time when journaled: the earliest a dispatch could start.
    pub(crate) journaled_at_ms: u64,
    /// The verified message carrying the destination, for the destination check before a
    /// repeat (Paykit spec 7.2 step 1). Neither it nor `carrier_record` in an entry kept
    /// before carriers were journaled: such an entry is never dispatched again.
    #[serde(default)]
    pub(crate) carrier: Option<KeptMessage>,
    /// The signed app record carrying a public endpoint, for the same check.
    #[serde(default)]
    pub(crate) carrier_record: Option<SignedDoc>,
    /// The payment method reported the dispatch failed, and `failed` was written on it: the
    /// method's history is asked again within its retention, and a payment that completed
    /// after all is reported with its proof (Paykit spec 7.2, "What a payment method
    /// declares").
    #[serde(default)]
    pub(crate) failed_by_method: bool,
    /// The attempt's transition proof, for the service and generation it was written at
    /// (Paykit spec 4, "Fresh before paying"). An entry without one is never dispatched again.
    #[serde(default)]
    pub(crate) attempt_proof: Option<SignedDoc>,
    /// The request paid, as the payer's request record stored it: with the acceptance and
    /// the proof, what rebuilds that record should it be gone by the outcome, so `done`
    /// never goes without it (Paykit spec 7.2 step 3). Absent in a payment without a
    /// request, and in an entry kept before requests were journaled, whose record is then
    /// never rebuilt: its outcome waits as for one that doesn't read.
    #[serde(default)]
    pub(crate) request: Option<KeptMessage>,
    /// The outcome of a payment made waits: the payer's activity record holds a value that
    /// doesn't read, which the SDK doesn't overwrite unasked, so neither `done` nor the
    /// proof is written. The attempt stays open, and nothing pays again. A durable notice
    /// for the host ([`PaymentView::record_held`]), until the record reads again or
    /// [`crate::Paykit::repair_activity_record`] writes it again.
    #[serde(default)]
    pub(crate) record_held: bool,
}

impl std::fmt::Debug for JournalEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JournalEntry({:?})", self.stage)
    }
}

impl JournalEntry {
    pub(crate) fn attempt(&self) -> Result<AttemptId> {
        AttemptId::parse(&self.attempt_id).map_err(|_| PaykitError::corrupt())
    }

    pub(crate) fn request_hash(&self) -> Result<Option<[u8; 32]>> {
        self.request_hash
            .as_deref()
            .map(|h| {
                pubky_switch_core::enc::unb64_array::<32>(h).map_err(|_| PaykitError::corrupt())
            })
            .transpose()
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
    ///   the request's; a payment without a request has neither, owes no acceptance, and its
    ///   claim key is the attempt's;
    /// - an acceptance is kept with its plaintext, which reads, and only when owed;
    /// - the method's proof is kept from `RailPaid` on, and before any payment time or
    ///   proof message; a `Finished` entry marked failed by the method holds no proof;
    /// - at most one carrier, which reads; the attempt's proof, when kept, reads;
    /// - a kept request only in a payment of a request, and it reads; an outcome held for
    ///   its record only once paid by the method.
    pub(crate) fn check(&self) -> Result<()> {
        let bad = || Err(PaykitError::corrupt());
        let attempt = self.attempt()?;
        let request_hash = self.request_hash()?;
        self.amount.amount()?;
        self.destination.check()?;
        self.label()?;
        match (&request_hash, &self.payment_reference) {
            (Some(hash), Some(reference)) => {
                PaymentReference::parse(reference).map_err(|_| PaykitError::corrupt())?;
                if self.natural_key != natural_key::request_record(&self.payee, hash) {
                    return bad();
                }
            }
            (None, None) => {
                if self.acceptance_owed
                    || self.destination.source()? == DestinationSource::Request
                    || self.obligation != payment_obligation(&attempt)
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
        if self.acceptance_cert.is_some() && self.acceptance_plaintext.is_none() {
            return bad();
        }
        let paid_members = self.paid_at_ms.is_some() || self.proof_msg_id.is_some();
        if (self.stage == Stage::RailPaid || paid_members) && self.rail_proof.is_none() {
            return bad();
        }
        if self.failed_by_method && self.rail_proof.is_some() {
            return bad();
        }
        if let Some(kept) = &self.request {
            if request_hash.is_none() {
                return bad();
            }
            kept.check_shape()?;
        }
        if self.record_held && self.rail_proof.is_none() {
            return bad();
        }
        // Owed: a proof, and an acceptance to queue or to place. One queued and not yet
        // placed is released and still owed here, as `finish_owed_acceptance` saves it
        // between the queueing and the record's batch.
        if self.stage == Stage::AcceptanceOwed
            && (self.rail_proof.is_none() || !self.acceptance_owed)
        {
            return bad();
        }
        match (&self.carrier, &self.carrier_record) {
            (Some(_), Some(_)) => return bad(),
            (Some(kept), None) => kept.check_shape()?,
            (None, Some(signed)) => {
                AppRecord::verify(signed, &self.payee).map_err(|_| PaykitError::corrupt())?;
            }
            (None, None) => {}
        }
        if let Some(signed) = &self.attempt_proof {
            TransitionProof::peek(signed).map_err(|_| PaykitError::corrupt())?;
        }
        Ok(())
    }

    /// [`JournalEntry::check`], and the entry's claim key bound to what it pays: for a
    /// payment of a request, the obligation `request_obligation` derives from the payee and
    /// the request, a one-time request's, Billing Period 0 (Paykit spec 7.1, Paykit wire
    /// 1.5); a payment without a request's is checked by `check` already. An entry whose
    /// claim key names another obligation would read and write one claim while its record
    /// and proof name another: it is a corrupt store.
    pub(crate) fn check_bound(
        &self,
        request_obligation: impl Fn(&[u8; 32], &[u8; 32]) -> [u8; 32],
    ) -> Result<()> {
        self.check()?;
        if let Some(hash) = self.request_hash()? {
            if self.obligation != request_obligation(&self.payee, &hash) {
                return Err(PaykitError::corrupt());
            }
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
    /// - The destination's carrier: the list message or app record, verified again.
    /// - The acceptance this app signed: read as its own message, under `own_cert`, to the
    ///   payee, an acceptance of that request with the `msg_id` the entry keeps.
    ///
    /// A corrupt store for any mismatch.
    pub(crate) fn check_restored(
        &self,
        own: &[u8; 32],
        own_cert: &AppCert,
        request_obligation: impl Fn(&[u8; 32], &[u8; 32]) -> [u8; 32],
    ) -> Result<()> {
        self.check_bound(request_obligation)?;
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
        }
        self.kept_carrier(own)?;
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
    /// it was signed under ([`JournalEntry::acceptance_cert`]), or `current` for an entry
    /// that kept none. It is read as any reader reads a stored message
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
        let cert = self.acceptance_cert.as_ref().unwrap_or(current.signed());
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

    /// The destination's carrier, verified again for `own`; `None` when the entry keeps none.
    pub(crate) fn kept_carrier(&self, own: &[u8; 32]) -> Result<Option<Carrier>> {
        if let Some(kept) = &self.carrier {
            return Ok(Some(Carrier::Message(Box::new(kept.stored(own)?))));
        }
        match &self.carrier_record {
            Some(signed) => AppRecord::verify(signed, &self.payee)
                .map(|record| Some(Carrier::Record(Box::new(record))))
                .map_err(|_| PaykitError::corrupt()),
            None => Ok(None),
        }
    }

    /// Whether the entry still has work: not finished, not abandoned.
    pub(crate) fn open(&self) -> bool {
        !matches!(
            self.stage,
            Stage::AcceptanceOwed | Stage::Finished | Stage::Abandoned
        )
    }

    /// Whether the payment method's call may have been made for this entry ([`dispatched`]).
    pub(crate) fn may_have_dispatched(&self) -> bool {
        dispatched(self.stage, self.restored)
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
pub struct PaymentView {
    /// The attempt ID, the payment method's idempotency key.
    pub attempt_id: AttemptId,
    /// The claim key of the obligation paid.
    pub obligation: [u8; 32],
    /// The payee.
    pub payee: [u8; 32],
    /// The request paid, `None` for a payment without one.
    pub request_hash: Option<[u8; 32]>,
    /// The amount.
    pub amount: PaymentAmount,
    /// The identifier paid.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// Where it stands.
    pub stage: Stage,
    /// The entry came back from a backup.
    pub restored: bool,
    /// Paid by the method, its outcome waits: the payer's activity record holds a value that
    /// doesn't read, so `done` and the proof aren't written, and the attempt stays open
    /// (Paykit spec 7.2 step 3). It settles once the record reads again, at
    /// [`crate::Paykit::resume_payments`], or when the user has the record written again
    /// with [`crate::Paykit::repair_activity_record`].
    pub record_held: bool,
}

impl JournalEntry {
    pub(crate) fn view(&self) -> Result<PaymentView> {
        Ok(PaymentView {
            attempt_id: self.attempt()?,
            obligation: self.obligation,
            payee: self.payee,
            request_hash: self.request_hash()?,
            amount: self.amount.amount()?,
            endpoint_id: self.destination.endpoint_id()?,
            stage: self.stage,
            restored: self.restored,
            record_held: self.record_held,
        })
    }
}

/// Whether the payment method's call may have been made, as the acceptance's release reads
/// it (Paykit spec 7.2 step 2): at `Paying`, journaled just before the call, or for a
/// restored entry not yet past it, whose dispatch the backup can't tell. Before `Paying` the
/// call is known not made; at `RailPaid` the method's history spoke (its paid branch releases
/// the acceptance), and at `RailFailed` it reported a definitive failure, so a later user's
/// `done` releases none.
pub(crate) fn dispatched(stage: Stage, restored: bool) -> bool {
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
    fn test_journal_dispatched_only_at_paying_or_restored_before_an_outcome() {
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
            assert_eq!(dispatched(stage, false), fresh, "{stage:?}");
            assert_eq!(dispatched(stage, true), restored, "{stage:?} restored");
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
