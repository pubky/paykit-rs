//! One-time payment requests and their answers (Paykit spec 3.4, 8.1; Paykit wire 4.5 to
//! 4.11): the payee sends a request and, once it has verified a proof, a receipt; the payer
//! sees the request and accepts it by paying, or rejects or cancels it; either side may
//! cancel.
//!
//! - **The payee's record** of a request is written in the batch that queues the request
//!   (Paykit spec 8.1 step 1): the request is signed first, and the record's key derives from
//!   its hash. The payee's receipt and cancellation go into the record the same way.
//! - **A payer's rejection or cancellation** is recorded in the request record, in the batch
//!   that queues it, conditioned on the request's claim having no open or paid attempt
//!   (`claim_step`); it goes out only once that batch is applied (Paykit spec 8.1). The batch
//!   writes the request record, so an attempt conditioned on the version read fails.
//! - **Labels:** a labelled selling app sends every request under its label, and the payer's
//!   replies copy it (Paykit spec 3.1, 3.3).

use paykit_lib::bodies::{
    PaymentDeadline, PaymentReceipt, PaymentRequest, RequestDestination, RequestReply,
};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
use paykit_lib::scopes::{PaykitScope, ScopeName};
use paykit_lib::{
    AttemptId, Body, Label, PaymentAmount, PaymentEndpointIdentifier, PaymentReference,
};
use pubky_switch_client::app::{claim_conflict, Delivery, OpBatch};
use pubky_switch_core::claims::{ClaimStatus, Outcome};
use pubky_switch_core::http::ErrorCode;
use serde_json::{Map, Value};

use crate::activity::{self, ActivityTarget, Role, SideTag};
use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::rail::ProofCheck;
use crate::records::{conflict, KeptMessage};
use crate::state::{key, Tx};

/// How many times a payee's write is built again after its record changed meanwhile.
const WRITE_TRIES: usize = 3;

/// A request, by its payee and its request hash (Paykit wire 1.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestRef {
    /// The payee identity.
    pub payee: [u8; 32],
    /// The request hash.
    pub request_hash: [u8; 32],
}

/// A one-time request to send (Paykit wire 4.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestDraft {
    /// The amount requested.
    pub amount: PaymentAmount,
    /// The reference copied into the payment where the method allows it.
    pub payment_reference: PaymentReference,
    /// Until when the payer may pay it; `None` for no expiry.
    pub proposal_expires_at_ms: Option<u64>,
    /// The payment deadline, `None` for none.
    pub payment_deadline_ms: Option<u64>,
    /// Where it is paid: endpoints of this app, an app of this identity, or any.
    pub destination: RequestDestination,
    /// The only identifiers the payer may use.
    pub accepted_endpoint_identifiers: Option<Vec<PaymentEndpointIdentifier>>,
    /// Opaque metadata.
    pub metadata: Option<Map<String, Value>>,
}

/// A request this app sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentRequest {
    /// The message ID.
    pub msg_id: String,
    /// Where the message is.
    pub delivery: Delivery,
    /// The request: its record is written in the batch that queues it, so it is known once
    /// the call returns.
    pub request: Option<RequestRef>,
}

/// What the payer sees for a request (Paykit spec 8.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestStatus {
    /// Nothing done yet.
    Open,
    /// This app's attempt is open.
    BeingPaidHere,
    /// "Being handled by your other wallet": another own app's attempt is open.
    BeingHandledByOtherWallet,
    /// An `unknown` or orphaned attempt: "may be paid" (Paykit spec 7.1).
    MayBePaid,
    /// SETTLED, or the record holds the proof of a payment: "Paid" only then.
    Paid,
    /// Rejected by this identity.
    Rejected,
    /// Cancelled by either side.
    Cancelled,
    /// Its proposal expired unpaid.
    Expired,
    /// Held after a restore: the payee may have cancelled it since the backup, and it is
    /// paid only once the user has seen that (Paykit spec 7.2,
    /// [`crate::PayOptions::possibly_cancelled_confirmed`]).
    Held,
}

/// A request the payer holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestView {
    /// The request.
    pub request: RequestRef,
    /// The record key of the payer's request record.
    pub record: [u8; 32],
    /// The amount requested.
    pub amount: PaymentAmount,
    /// The payment reference.
    pub payment_reference: String,
    /// The label of the selling scope it was sent under.
    pub label: Option<String>,
    /// The proposal expiry.
    pub proposal_expires_at_ms: Option<u64>,
    /// Whether it is recurring.
    pub recurring: bool,
    /// What the user sees.
    pub status: RequestStatus,
}

/// What the payee sees of a request it sent or a payment it received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PayeeStatus {
    /// Sent; no answer yet.
    Requested,
    /// Accepted: the payer is paying.
    Accepted,
    /// Rejected by the payer.
    Rejected,
    /// Cancelled by either side.
    Cancelled,
    /// A proof arrived; the payee verifies it by its own method.
    ProofReceived,
    /// The payee sent its receipt.
    Receipted,
}

/// A request this identity sent, or a payment without a request it received, as the payee
/// sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayeeActivity {
    /// The record key of the payee's record.
    pub record: [u8; 32],
    /// The payer.
    pub payer: [u8; 32],
    /// The request, `None` for a payment without one.
    pub request_hash: Option<[u8; 32]>,
    /// The amount: the request's, or the proof's for a payment without a request.
    pub amount: Option<PaymentAmount>,
    /// The label the request was sent under.
    pub label: Option<String>,
    /// The attempt the proof reports.
    pub attempt_id: Option<AttemptId>,
    /// Where it stands.
    pub status: PayeeStatus,
}

/// The request in a record, with its body.
pub(crate) fn request_of(record: &ActivityRecord) -> Option<(&StoredMessage, &PaymentRequest)> {
    let stored = record.request.as_ref()?;
    match stored.body() {
        Body::PaymentRequest(request) => Some((stored, request)),
        _ => None,
    }
}

/// The record holds this identity's rejection (Paykit spec 8.1). Status comes from the
/// messages held, never from `status` (Paykit wire 5.3).
pub(crate) fn rejected(record: &ActivityRecord) -> bool {
    matches!(
        record.response.as_ref().map(StoredMessage::body),
        Some(Body::PaymentRequestRejection(_))
    )
}

/// The record holds a cancellation from either side (Paykit spec 8.1).
pub(crate) fn cancelled(record: &ActivityRecord) -> bool {
    record.cancellation.is_some()
}

impl Paykit {
    // ---------------------------------------------------------------------------------
    // The payee

    /// The label this app sends requests under (Paykit spec 3.1): the configured one, which
    /// it must hold; for an app holding only labelled scopes, its one label when it holds
    /// exactly one.
    fn selling_label(&self) -> Result<Option<Label>> {
        let scopes = self.scopes();
        let plain = scopes.iter().any(|s| s == "request");
        let labels: Vec<Label> = scopes
            .iter()
            .filter_map(|s| match PaykitScope::classify(s) {
                ScopeName::Paykit(PaykitScope::RequestLabel(label)) => Some(label),
                _ => None,
            })
            .collect();
        match (&self.config.selling_label, plain) {
            (Some(label), true) => Ok(Some(label.clone())),
            (Some(label), false) if labels.contains(label) => Ok(Some(label.clone())),
            (Some(_), false) => Err(Refusal::Label(
                "the configured label isn't one this certificate holds".into(),
            )
            .into()),
            (None, true) => Ok(None),
            (None, false) => match labels.as_slice() {
                [only] => Ok(Some(only.clone())),
                _ => Err(Refusal::Label(
                    "an app holding only labelled scopes sends every request under one of them"
                        .into(),
                )
                .into()),
            },
        }
    }

    /// Send a one-time request to `to` (Paykit spec 8.1 step 1, Paykit wire 4.5), under this
    /// app's selling label. The payer must admit this identity, for example after an invite
    /// (Paykit spec 1). The request is signed, and its record written in the batch that
    /// queues it, then it is sent.
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `request` or a label; [`Refusal::Label`];
    /// [`PaykitError::Lib`] with `Validation` for a draft that breaks Paykit wire 4.5; the
    /// client's errors.
    pub async fn send_request(&mut self, to: [u8; 32], draft: RequestDraft) -> Result<SentRequest> {
        self.refresh_own()?;
        self.require_payee()?;
        let label = self.selling_label()?;
        let request = PaymentRequest {
            amount: draft.amount,
            payment_reference: draft.payment_reference,
            proposal_expires_at: draft.proposal_expires_at_ms,
            payment_deadline: draft.payment_deadline_ms.map(PaymentDeadline::At),
            recurrence: None,
            destination: draft.destination,
            per_period_endpoints: false,
            label: label.clone(),
            accepted_payment_endpoint_identifiers: draft.accepted_endpoint_identifiers,
            conversion: None,
            metadata: draft.metadata,
        };
        // The writer reads back what it writes by the recipient's rules (Paykit wire 4.5).
        let body = request.to_body()?;
        let signed = self.sign(to, Kind::PaymentRequest, Vec::new(), body)?;
        let msg_id = signed.msg_id().to_owned();
        let hash = signed.hash();
        // The payee's request record, from the signed request, in the batch that queues it
        // (Paykit spec 8.1 step 1): its key derives from the request's hash.
        let own = self.identity();
        let target =
            ActivityTarget::payee(natural_key::request_record(&own, &hash), label.as_ref());
        let mut record = activity::empty(ActivitySide::Payee, to);
        activity::place(&mut record, Role::Request, self.own_copy(&signed)?);
        let value = record.to_json()?;
        let write = self
            .app
            .record_op(&target.record_target(), Some(&value), Vec::new())
            .await?;
        let mut tx = Tx::default();
        self.note_peer(&mut tx, &to)?;
        let batch = OpBatch::new()
            .entry(write)
            .queue(signed)
            .locals(tx.into_writes());
        self.app.submit_batch(batch).await?;
        let delivery = self
            .app
            .flush_message(&msg_id)
            .await?
            .ok_or_else(PaykitError::corrupt)?;
        Ok(SentRequest {
            msg_id,
            delivery,
            request: Some(RequestRef {
                payee: own,
                request_hash: hash,
            }),
        })
    }

    /// The payee's record at `record`, bound under its policy, with its target.
    async fn payee_record(&self, record: &[u8; 32]) -> Result<(ActivityTarget, ActivityRecord)> {
        let held = self
            .app
            .shared_record(record)
            .await?
            .ok_or(Refusal::NotFound)?;
        let label = Collection::PayeeActivity
            .policy_label(&held.policy)
            .ok_or(Refusal::NotFound)?;
        let value = held.content.value().ok_or(Refusal::NotFound)?;
        let bound = crate::records::bind_activity(
            value,
            &self.identity(),
            ActivitySide::Payee,
            label.as_ref(),
            record,
            |t| self.app.record_key(t).ok(),
            &|p| self.rail.settlement_id(p),
        )
        .ok_or(Refusal::NotFound)?;
        let target = ActivityTarget::payee(
            bound.natural_key_with(&self.identity(), &|p| self.rail.settlement_id(p))?,
            label.as_ref(),
        );
        Ok((target, bound))
    }

    /// Every request this identity sent and payment without a request it received, from the
    /// `payee_activity` records this app holds (Paykit spec 6). A record that doesn't bind is
    /// left out, as a reader doesn't act on it (Paykit wire 5.3).
    ///
    /// # Errors
    ///
    /// A store failure, also one reading a single record: the list is never partial.
    pub async fn payee_activity(&self) -> Result<Vec<PayeeActivity>> {
        let mut out = Vec::new();
        for held in self
            .app
            .records_of(Collection::PayeeActivity.name())
            .await?
        {
            match self.payee_record(&held.key).await {
                Ok((_, record)) => out.push(payee_view(held.key, &record)),
                // A record that doesn't bind, or isn't held under a Paykit policy, isn't acted
                // on (Paykit wire 5.3): it is left out. A failure to read it is not.
                Err(PaykitError::Refused(Refusal::NotFound) | PaykitError::Lib(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(out)
    }

    /// The payee's own check of the proof in `record` (Paykit spec 8.1 step 4): the verified
    /// payment time, or `None` when the payment method doesn't confirm it.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for a record without a proof; the rail's errors.
    pub async fn verify_payment(&mut self, record: &[u8; 32]) -> Result<Option<u64>> {
        let (_, bound) = self.payee_record(record).await?;
        let proof_stored = bound.proof.as_ref().ok_or(Refusal::NotFound)?;
        let Body::PaymentProof(proof) = proof_stored.body() else {
            return Err(Refusal::NotFound.into());
        };
        let (amount, payload) = match request_of(&bound) {
            Some((_, request)) => {
                let payload = match &request.destination {
                    RequestDestination::Endpoints(endpoints) => {
                        endpoints.get(&proof.payment_endpoint_identifier).cloned()
                    }
                    _ => None,
                };
                (request.amount.clone(), payload)
            }
            None => (proof.amount.clone().ok_or(Refusal::NotFound)?, None),
        };
        let check = ProofCheck {
            endpoint_id: proof.payment_endpoint_identifier.clone(),
            payload,
            amount,
            attempt_id: proof.attempt_id,
            proof: proof.proof.clone(),
        };
        Ok(self.rail.verify(check).await?)
    }

    /// Verify the proof in `record` and, when the payment method confirms it, send the
    /// payer a receipt (Paykit spec 8.1 step 4, Paykit wire 4.11). Returns the receipt's
    /// `msg_id`, or `None` when the method doesn't confirm the payment.
    ///
    /// # Errors
    ///
    /// As [`Paykit::verify_payment`]; the client's errors.
    pub async fn confirm_payment(&mut self, record: &[u8; 32]) -> Result<Option<String>> {
        self.refresh_own()?;
        if self.verify_payment(record).await?.is_none() {
            return Ok(None);
        }
        let mut tries = 0;
        let msg_id = loop {
            let (target, bound) = self.payee_record(record).await?;
            if let Some(receipt) = &bound.receipt {
                return Ok(Some(receipt.message().msg_id.clone()));
            }
            let proof_stored = bound.proof.as_ref().ok_or(Refusal::NotFound)?;
            let Body::PaymentProof(proof) = proof_stored.body() else {
                return Err(Refusal::NotFound.into());
            };
            let mut refs = vec![proof_stored.hash()];
            if let Some((request, _)) = request_of(&bound) {
                refs.push(request.hash());
            }
            // The receipt carries the proof's attempt and Billing Period (Paykit wire 4.11).
            let body = PaymentReceipt {
                attempt_id: proof.attempt_id,
                billing_period: proof.billing_period,
            }
            .to_body(refs.len())?;
            // The receipt goes into the payee's record in the batch that queues it.
            let signed = self.sign(bound.counterparty, Kind::PaymentReceipt, refs, body)?;
            match self
                .send_with_record(signed, &target, Role::Receipt, Vec::new(), Vec::new())
                .await
            {
                Ok(msg_id) => break msg_id,
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    tries += 1;
                    if tries >= WRITE_TRIES {
                        return Err(conflict(error));
                    }
                }
                Err(error) => return Err(error),
            }
        };
        self.app.flush_message(&msg_id).await?;
        Ok(Some(msg_id))
    }

    /// The payee cancels a request it sent (Paykit spec 8.1, Paykit wire 4.8). The
    /// cancellation carries no label and goes to the payer's apps holding `pay` or
    /// `request` (Paykit spec 3.4). A proof that crosses it still counts as payment.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`]; [`Refusal::Cancelled`] if it was already; the client's errors.
    pub async fn cancel_sent_request(
        &mut self,
        record: &[u8; 32],
        reason: Option<String>,
    ) -> Result<String> {
        self.refresh_own()?;
        self.require_payee()?;
        let mut tries = 0;
        let msg_id = loop {
            let (target, bound) = self.payee_record(record).await?;
            let (request, _) = request_of(&bound).ok_or(Refusal::NotFound)?;
            if bound.cancellation.is_some() {
                return Err(Refusal::Cancelled.into());
            }
            let body = RequestReply {
                reason: reason.clone(),
                label: None,
            }
            .to_body()?;
            let signed = self.sign(
                bound.counterparty,
                Kind::PaymentRequestCancellation,
                vec![request.hash()],
                body,
            )?;
            // The side it is sent from goes with it, for the label rule of senders (Paykit
            // spec 3.4).
            let mut tx = Tx::default();
            tx.put(&key::side(signed.msg_id()), &SideTag::Payee)?;
            match self
                .send_with_record(
                    signed,
                    &target,
                    Role::Cancellation,
                    Vec::new(),
                    tx.into_writes(),
                )
                .await
            {
                Ok(msg_id) => break msg_id,
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    tries += 1;
                    if tries >= WRITE_TRIES {
                        return Err(conflict(error));
                    }
                }
                Err(error) => return Err(error),
            }
        };
        self.app.flush_message(&msg_id).await?;
        Ok(msg_id)
    }

    // ---------------------------------------------------------------------------------
    // The payer

    /// The payer's request record of `request`, read from the switch when not held. A
    /// cancellation this app kept for it (`receive.rs`) is placed in the record first, if it
    /// can be: another own app may have written the record from a copy of the request it got
    /// without the cancellation (Paykit spec 8.1). A placement that fails changes nothing
    /// here: the kept stop still stands, and [`Paykit::kept_stop`] refuses the payment.
    pub(crate) async fn payer_request(
        &mut self,
        request: &RequestRef,
    ) -> Result<(ActivityTarget, ActivityRecord)> {
        let target = ActivityTarget::payer(natural_key::request_record(
            &request.payee,
            &request.request_hash,
        ));
        let record = self.activity(&target).await?.ok_or(Refusal::NotFound)?;
        if request_of(&record).is_none() {
            return Err(Refusal::NotFound.into());
        }
        // The kept stop refuses whatever this placement does; it is tried again later.
        if self.place_kept_cancellation(&target).await.unwrap_or(false) {
            let record = self.activity(&target).await?.ok_or(Refusal::NotFound)?;
            return Ok((target, record));
        }
        Ok((target, record))
    }

    /// Place the cancellation kept for the request record at `target`, if any, and drop the
    /// kept copy, in one batch. Returns whether there was one.
    async fn place_kept_cancellation(&mut self, target: &ActivityTarget) -> Result<bool> {
        let kept_key = key::kept_cancellation(&self.activity_key(target)?);
        let Some(kept) = self.local_get::<KeptMessage>(&kept_key).await? else {
            return Ok(false);
        };
        let cancellation = kept.stored(&self.identity())?;
        let write = self
            .placed_write(
                target,
                cancellation.sender(),
                Role::Cancellation,
                cancellation,
                Vec::new(),
            )
            .await?;
        let mut tx = Tx::default();
        tx.delete(&kept_key);
        match write {
            Some(entry) => {
                let batch = OpBatch::new().entry(entry).locals(tx.into_writes());
                if let Err(error) = self.app.submit_batch(batch).await {
                    if error.code() == Some(ErrorCode::Conflict) {
                        self.app.read_record(&target.record_target()).await?;
                    }
                    return Err(conflict(error.into()));
                }
            }
            None => self.local_commit(tx).await?,
        }
        Ok(true)
    }

    /// Whether this app keeps a payee's cancellation for the request record at `target`, the
    /// kept-copy stop (Paykit spec 8.1): read by every path that makes a new attempt on the
    /// request, so a stop not yet placed in the record still refuses it.
    pub(crate) async fn kept_stop(&self, target: &ActivityTarget) -> Result<bool> {
        let kept_key = key::kept_cancellation(&self.activity_key(target)?);
        Ok(self.local_get::<KeptMessage>(&kept_key).await?.is_some())
    }

    /// Place every kept cancellation whose request record this app holds, each as a later
    /// write of its own with the kept copy dropped (Paykit spec 8.1). Run after each
    /// receive, at a resume, and before any payment. One that can't be placed now stays
    /// kept, for the next run; its kept copy refuses every new attempt meanwhile.
    pub(crate) async fn place_kept_cancellations(&mut self) -> Result<()> {
        let own = self.identity();
        for (_, kept) in self
            .local_scan::<KeptMessage>(key::KEPT_CANCELLATIONS)
            .await?
        {
            let cancellation = kept.stored(&own)?;
            let Some(request) = cancellation.message().refs.first().copied() else {
                continue;
            };
            let target = ActivityTarget::payer(natural_key::request_record(
                &cancellation.sender(),
                &request,
            ));
            let held = self
                .held_activity(&target)
                .await?
                .is_some_and(|record| request_of(&record).is_some());
            if held {
                // A placement that fails now is tried again at the next run; the kept stop
                // stands meanwhile, so it can't let a payment through.
                let _ = self.place_kept_cancellation(&target).await;
            }
        }
        Ok(())
    }

    /// Every request the payer's apps received, from the `payer_activity` records this app
    /// holds, with what the user sees for each (Paykit spec 8.1 step 2, 8.5).
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn requests(&self) -> Result<Vec<RequestView>> {
        let own = self.identity();
        let mut out = Vec::new();
        for held in self
            .app
            .records_of(Collection::PayerActivity.name())
            .await?
        {
            let Some(value) = held.content.value() else {
                continue;
            };
            let Some(record) = crate::records::bind_activity(
                value,
                &own,
                ActivitySide::Payer,
                None,
                &held.key,
                |t| self.app.record_key(t).ok(),
                &|p| self.rail.settlement_id(p),
            ) else {
                continue;
            };
            if let Some(view) = self.request_view(held.key, &record).await? {
                out.push(view);
            }
        }
        Ok(out)
    }

    /// One request, as [`Paykit::requests`] shows it.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`]; a store failure.
    pub async fn request(&self, request: &RequestRef) -> Result<RequestView> {
        let target = ActivityTarget::payer(natural_key::request_record(
            &request.payee,
            &request.request_hash,
        ));
        let record = self
            .held_activity(&target)
            .await?
            .ok_or(Refusal::NotFound)?;
        let key = self.activity_key(&target)?;
        self.request_view(key, &record)
            .await?
            .ok_or_else(|| Refusal::NotFound.into())
    }

    async fn request_view(
        &self,
        record_key: [u8; 32],
        record: &ActivityRecord,
    ) -> Result<Option<RequestView>> {
        let Some((stored, request)) = request_of(record) else {
            return Ok(None);
        };
        let payee = record.counterparty;
        let hash = stored.hash();
        let mut status = self.request_status(record, request, &payee, &hash).await?;
        // A cancellation kept for the request and not placed yet is a cancellation all the
        // same (Paykit spec 8.1).
        if status == RequestStatus::Open
            && self
                .local_get::<KeptMessage>(&key::kept_cancellation(&record_key))
                .await?
                .is_some()
        {
            status = RequestStatus::Cancelled;
        }
        if status == RequestStatus::Open && self.held(&record_key, record).await? {
            status = RequestStatus::Held;
        }
        Ok(Some(RequestView {
            request: RequestRef {
                payee,
                request_hash: hash,
            },
            record: record_key,
            amount: request.amount.clone(),
            payment_reference: request.payment_reference.as_str().to_owned(),
            label: request.label.as_ref().map(|l| l.as_str().to_owned()),
            proposal_expires_at_ms: request.proposal_expires_at,
            recurring: request.is_recurring(),
            status,
        }))
    }

    async fn request_status(
        &self,
        record: &ActivityRecord,
        request: &PaymentRequest,
        payee: &[u8; 32],
        hash: &[u8; 32],
    ) -> Result<RequestStatus> {
        if record.shows_paid() {
            return Ok(RequestStatus::Paid);
        }
        let obligation = self.obligation(payee, hash, 0);
        if let Some(evidence) = self.app.claim_evidence(&obligation).await? {
            let view = evidence.view;
            match view.status {
                ClaimStatus::Settled => return Ok(RequestStatus::Paid),
                ClaimStatus::Attempting => {
                    return Ok(if view.owner == Some(self.app_id()) {
                        RequestStatus::BeingPaidHere
                    } else {
                        RequestStatus::BeingHandledByOtherWallet
                    });
                }
                ClaimStatus::Free => {
                    let doubtful = self
                        .app
                        .claim_doubt(&obligation)
                        .await?
                        .is_some_and(|d| d.doubtful())
                        || view.attempts.iter().any(|a| a.outcome == Outcome::Unknown);
                    if doubtful {
                        return Ok(RequestStatus::MayBePaid);
                    }
                }
            }
        }
        if rejected(record) {
            return Ok(RequestStatus::Rejected);
        }
        if cancelled(record) {
            return Ok(RequestStatus::Cancelled);
        }
        // Expired once the deadline is at or before the highest service time verified (Paykit
        // spec 7.1).
        if request
            .attempt_deadline(None)
            .ok()
            .flatten()
            .is_some_and(|deadline| self.service_time().is_some_and(|t| deadline <= t))
        {
            return Ok(RequestStatus::Expired);
        }
        Ok(RequestStatus::Open)
    }

    /// Reject a request (Paykit spec 8.1, Paykit wire 4.7): recorded first, conditioned on
    /// its claim having no open or paid attempt, then sent. Returns the rejection's `msg_id`.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`]; [`Refusal::Rejected`], [`Refusal::Cancelled`],
    /// [`Refusal::AlreadyPaid`] or [`Refusal::PaymentInProgress`]; [`Refusal::Recurring`];
    /// [`PaykitError::Conflict`] when the record changed meanwhile; the client's errors.
    pub async fn reject_request(
        &mut self,
        request: &RequestRef,
        reason: Option<String>,
    ) -> Result<String> {
        self.refresh_own()?;
        self.answer_request(request, false, reason).await
    }

    /// The payer cancels a request (Paykit spec 8.1, Paykit wire 4.8), recorded and sent as
    /// a rejection is. Returns the cancellation's `msg_id`.
    ///
    /// # Errors
    ///
    /// As [`Paykit::reject_request`].
    pub async fn cancel_request(
        &mut self,
        request: &RequestRef,
        reason: Option<String>,
    ) -> Result<String> {
        self.refresh_own()?;
        self.answer_request(request, true, reason).await
    }

    async fn answer_request(
        &mut self,
        request: &RequestRef,
        cancel: bool,
        reason: Option<String>,
    ) -> Result<String> {
        self.require_pay()?;
        let (target, record) = self.payer_request(request).await?;
        let (_, body) = request_of(&record).ok_or(Refusal::NotFound)?;
        if body.is_recurring() {
            return Err(Refusal::Recurring.into());
        }
        let label = body.label.clone();
        if record.shows_paid() {
            return Err(Refusal::AlreadyPaid.into());
        }
        if rejected(&record) {
            return Err(Refusal::Rejected.into());
        }
        if cancelled(&record) || self.kept_stop(&target).await? {
            return Err(Refusal::Cancelled.into());
        }
        let obligation = self.obligation(&request.payee, &request.request_hash, 0);
        if let Some(read) = self.app.read_claim(&obligation).await? {
            refuse_on_claim(&read.view)?;
        }
        let reply = RequestReply { reason, label };
        let (kind, role) = if cancel {
            (Kind::PaymentRequestCancellation, Role::Cancellation)
        } else {
            (Kind::PaymentRequestRejection, Role::Response)
        };
        let signed = self.sign(
            request.payee,
            kind,
            vec![request.request_hash],
            reply.to_body()?,
        )?;
        let mut tx = Tx::default();
        if cancel {
            tx.put(&key::side(signed.msg_id()), &SideTag::Payer)?;
        }
        // Recorded in the request record in the batch that queues it, conditioned on the
        // claim at the step read: an attempt meanwhile fails the batch, and the reply goes
        // nowhere (Paykit spec 8.1).
        let condition = self.app.claim_condition(&obligation).await?;
        let msg_id = match self
            .send_with_record(signed, &target, role, vec![condition], tx.into_writes())
            .await
        {
            Ok(msg_id) => msg_id,
            Err(error) => {
                if let PaykitError::Client(client) = &error {
                    if let Some(view) = claim_conflict(client) {
                        refuse_on_claim(&view)?;
                    }
                }
                return Err(conflict(error));
            }
        };
        self.app.flush_message(&msg_id).await?;
        Ok(msg_id)
    }
}

/// A rejection or cancellation is refused while the claim has an open or paid attempt
/// (Paykit spec 8.1).
fn refuse_on_claim(view: &pubky_switch_core::claims::ClaimView) -> Result<()> {
    match view.status {
        ClaimStatus::Settled => Err(Refusal::AlreadyPaid.into()),
        ClaimStatus::Attempting => Err(Refusal::PaymentInProgress.into()),
        ClaimStatus::Free => {
            if view.attempts.iter().any(|a| a.outcome == Outcome::Unknown) {
                Err(Refusal::MayBePaid("an attempt's outcome is unknown".into()).into())
            } else {
                Ok(())
            }
        }
    }
}

fn payee_view(record_key: [u8; 32], record: &ActivityRecord) -> PayeeActivity {
    let request = request_of(record);
    let proof = match record.proof.as_ref().map(StoredMessage::body) {
        Some(Body::PaymentProof(proof)) => Some(proof),
        _ => None,
    };
    let rejected = matches!(
        record.response.as_ref().map(StoredMessage::body),
        Some(Body::PaymentRequestRejection(_))
    );
    let status = if record.receipt.is_some() {
        PayeeStatus::Receipted
    } else if record.proof.is_some() {
        PayeeStatus::ProofReceived
    } else if record.cancellation.is_some() {
        PayeeStatus::Cancelled
    } else if rejected {
        PayeeStatus::Rejected
    } else if record.response.is_some() {
        PayeeStatus::Accepted
    } else {
        PayeeStatus::Requested
    };
    PayeeActivity {
        record: record_key,
        payer: record.counterparty,
        request_hash: request.map(|(stored, _)| stored.hash()),
        amount: request
            .map(|(_, r)| r.amount.clone())
            .or_else(|| proof.and_then(|p| p.amount.clone())),
        label: request.and_then(|(_, r)| r.label.as_ref().map(|l| l.as_str().to_owned())),
        attempt_id: proof.map(|p| p.attempt_id),
        status,
    }
}

#[cfg(test)]
mod tests {
    use pubky_switch_core::claims::ClaimView;
    use serde_json::json;

    use super::*;

    fn view(state: &str, outcome: &str) -> ClaimView {
        let attempt = json!({ "attempt_id": "1OjeFqsP4dPATeMe9xu5-Q", "duplicate": false,
            "opened_at": "2026-01-01T00:00:00.000Z", "orphan_after": 86400, "orphaned": false,
            "outcome": outcome, "owner": "4ppx81nsrtmpoke6yjfueg957h", "possibly_duplicate": false });
        let owner = if state == "ATTEMPTING" {
            json!("4ppx81nsrtmpoke6yjfueg957h")
        } else {
            json!(null)
        };
        let attempt_id = if state == "ATTEMPTING" {
            json!("1OjeFqsP4dPATeMe9xu5-Q")
        } else {
            json!(null)
        };
        ClaimView::from_value(&json!({
            "attempt_id": attempt_id, "attempts": [attempt],
            "claim_key": "QZGfAHpm0NnUuyVE9zAHkr-1o0_2ofsJwVJJ2KQ8NCU",
            "deadline": null, "divergent": false, "next_possibly_duplicate": false,
            "owner": owner, "state": state, "step": 1, "tombstone": false
        }))
        .unwrap()
    }

    #[test]
    fn test_requests_answer_allowed_on_a_free_claim() {
        assert!(refuse_on_claim(&view("FREE", "failed")).is_ok());
    }

    #[test]
    fn test_requests_answer_refused_on_an_open_paid_or_unknown_attempt() {
        let refusal = |v: ClaimView| refuse_on_claim(&v).unwrap_err().refusal().cloned();
        assert_eq!(
            refusal(view("ATTEMPTING", "open")),
            Some(Refusal::PaymentInProgress)
        );
        assert_eq!(refusal(view("SETTLED", "done")), Some(Refusal::AlreadyPaid));
        assert!(matches!(
            refusal(view("FREE", "unknown")),
            Some(Refusal::MayBePaid(_))
        ));
    }
}
