//! One-time payment requests and their answers (Paykit spec 3.4, 8.1; Paykit wire 4.5 to
//! 4.11): the payee sends a request and, once it has verified a proof, a receipt; the payer
//! sees the request and accepts it by paying, or rejects or cancels it; either side may
//! cancel.
//!
//! - **The payee's record** of a request is written once the payer's inbox has accepted it,
//!   from the signed copy the client keeps: the record's key derives from the request's hash,
//!   which exists only once the request is signed.
//! - **A payer's rejection or cancellation** is recorded first, in a batch conditioned on the
//!   request's claim having no open or paid attempt (`claim_step`), and sent only after that
//!   batch succeeds (Paykit spec 8.1). The batch writes the request record, so an attempt
//!   conditioned on the version read fails; the stored copy joins the record when the
//!   message is delivered.
//! - **Labels:** a labelled selling app sends every request under its label, and the payer's
//!   replies copy it (Paykit spec 3.1, 3.3).

use paykit_lib::bodies::{
    PaymentDeadline, PaymentReceipt, PaymentRequest, PaymentRequestAcceptance, RequestDestination,
    RequestReply,
};
use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, request_obligation};
use paykit_lib::kinds::Kind;
use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
use paykit_lib::scopes::{PaykitScope, ScopeName};
use paykit_lib::{
    AttemptId, Body, Label, PaymentAmount, PaymentEndpointIdentifier, PaymentReference,
};
use pubky_switch_client::app::{claim_conflict, Delivery};
use pubky_switch_core::claims::{ClaimStatus, Outcome};
use pubky_switch_core::enc::serde_b64;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::activity::{ActivityTarget, Role, SideTag};
use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::rail::ProofCheck;
use crate::state::{key, Tx};

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
    /// The request, once the payer's inbox accepted it and its record is written.
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

/// A request sent whose record is still to be written (`p/req/<msg_id>`).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingRequest {
    #[serde(with = "serde_b64")]
    to: [u8; 32],
    label: Option<String>,
}

/// A payer's rejection or cancellation recorded and still to be sent (`p/reply/<record>`).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct OwedReply {
    #[serde(with = "serde_b64")]
    payee: [u8; 32],
    #[serde(with = "serde_b64")]
    request_hash: [u8; 32],
    cancel: bool,
    reason: Option<String>,
    label: Option<String>,
}

/// The request in a record, with its body.
pub(crate) fn request_of(record: &ActivityRecord) -> Option<(&StoredMessage, &PaymentRequest)> {
    let stored = record.request.as_ref()?;
    match stored.body() {
        Body::PaymentRequest(request) => Some((stored, request)),
        _ => None,
    }
}

fn rejected(record: &ActivityRecord) -> bool {
    matches!(
        record.response.as_ref().map(StoredMessage::body),
        Some(Body::PaymentRequestRejection(_))
    ) || record.status.as_deref() == Some("rejected")
}

fn cancelled(record: &ActivityRecord) -> bool {
    record.cancellation.is_some() || record.status.as_deref() == Some("cancelled")
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
    /// (Paykit spec 1). The record is written once the payer's inbox accepts the request.
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `request` or a label; [`Refusal::Label`];
    /// [`PaykitError::Lib`] with `Validation` for a draft that breaks Paykit wire 4.5; the
    /// client's errors.
    pub async fn send_request(&mut self, to: [u8; 32], draft: RequestDraft) -> Result<SentRequest> {
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
        let body = request.to_body();
        PaymentRequest::from_body(&body).map_err(paykit_lib::Error::into_validation)?;
        let msg_id = self
            .queue_message(to, Kind::PaymentRequest, Vec::new(), body, None)
            .await?;
        let mut tx = Tx::default();
        tx.put(
            &key::request(&msg_id),
            &PendingRequest {
                to,
                label: label.map(|l| l.as_str().to_owned()),
            },
        )?;
        self.note_peer(&mut tx, &to)?;
        self.local.commit(tx).await?;
        Box::pin(self.app.flush_outbox()).await?;
        let written = self.write_request_records().await?;
        let delivery = self
            .app
            .delivery(&msg_id)
            .await?
            .ok_or_else(PaykitError::corrupt)?;
        let request = written
            .into_iter()
            .find(|(id, _)| *id == msg_id)
            .map(|(_, hash)| RequestRef {
                payee: self.identity(),
                request_hash: hash,
            });
        Ok(SentRequest {
            msg_id,
            delivery,
            request,
        })
    }

    /// Write the payee's record of each request the payer's inbox accepted since (Paykit
    /// spec 8.1 step 1). Returns the `msg_id`s written, with their request hashes.
    pub(crate) async fn write_request_records(&mut self) -> Result<Vec<(String, [u8; 32])>> {
        let own = self.identity();
        let mut written = Vec::new();
        for (msg_id, pending) in self.local.scan::<PendingRequest>(key::REQUESTS).await? {
            match self.app.delivery(&msg_id).await? {
                Some(Delivery::Accepted { accepted_at_ms }) => {
                    let sent = self
                        .app
                        .sent_to(&pending.to, accepted_at_ms, accepted_at_ms)
                        .await?
                        .into_iter()
                        .find(|s| s.msg_id == msg_id);
                    if let Some(sent) = sent {
                        let stored = StoredMessage::new(
                            own,
                            self.app.cert().signed(),
                            sent.plaintext,
                            Some(accepted_at_ms),
                            &pending.to,
                        )?;
                        let hash = stored.hash();
                        let label = pending.label.as_deref().and_then(|l| Label::parse(l).ok());
                        let target = ActivityTarget::payee(
                            natural_key::request_record(&own, &hash),
                            label.as_ref(),
                        );
                        self.place(&target, pending.to, Role::Request, stored, Vec::new())
                            .await?;
                        written.push((msg_id.clone(), hash));
                    }
                    self.local.delete(&key::request(&msg_id)).await?;
                }
                Some(Delivery::Queued { .. }) => {}
                Some(Delivery::Failed { .. }) | None => {
                    self.local.delete(&key::request(&msg_id)).await?;
                }
            }
        }
        Ok(written)
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
        let bound = ActivityRecord::bind(
            value,
            &self.identity(),
            ActivitySide::Payee,
            self.naming(),
            record,
            label.as_ref(),
        )
        .map_err(|_| Refusal::NotFound)?;
        let target = ActivityTarget::payee(bound.natural_key(&self.identity())?, label.as_ref());
        Ok((target, bound))
    }

    /// Every request this identity sent and payment without a request it received, from the
    /// `payee_activity` records this app holds (Paykit spec 6).
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn payee_activity(&self) -> Result<Vec<PayeeActivity>> {
        let mut out = Vec::new();
        for held in self
            .app
            .records_of(Collection::PayeeActivity.name())
            .await?
        {
            let Ok((_, record)) = self.payee_record(&held.key).await else {
                continue;
            };
            out.push(payee_view(held.key, &record));
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
        if self.verify_payment(record).await?.is_none() {
            return Ok(None);
        }
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
        let body = PaymentReceipt {
            attempt_id: proof.attempt_id,
            billing_period: proof.billing_period,
        }
        .to_body();
        let msg_id = self
            .queue_message(
                bound.counterparty,
                Kind::PaymentReceipt,
                refs,
                body,
                Some((&target, Role::Receipt)),
            )
            .await?;
        Box::pin(self.app.flush_outbox()).await?;
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
        self.require_payee()?;
        let (target, bound) = self.payee_record(record).await?;
        let (request, _) = request_of(&bound).ok_or(Refusal::NotFound)?;
        if bound.cancellation.is_some() {
            return Err(Refusal::Cancelled.into());
        }
        let body = RequestReply {
            reason,
            label: None,
        }
        .to_body();
        let msg_id = self
            .queue_message(
                bound.counterparty,
                Kind::PaymentRequestCancellation,
                vec![request.hash()],
                body,
                Some((&target, Role::Cancellation)),
            )
            .await?;
        self.local.put(&key::side(&msg_id), &SideTag::Payee).await?;
        Box::pin(self.app.flush_outbox()).await?;
        Ok(msg_id)
    }

    // ---------------------------------------------------------------------------------
    // The payer

    /// The payer's request record of `request`, read from the switch when not held.
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
        Ok((target, record))
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
            let Ok(record) = ActivityRecord::bind(
                value,
                &own,
                ActivitySide::Payer,
                self.naming(),
                &held.key,
                None,
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
        let status = self.request_status(record, request, &payee, &hash).await?;
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
        let obligation = request_obligation(self.naming(), payee, hash, 0);
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
        if request
            .attempt_deadline(None)
            .ok()
            .flatten()
            .is_some_and(|deadline| deadline <= self.now())
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
        if cancelled(&record) {
            return Err(Refusal::Cancelled.into());
        }
        let obligation =
            request_obligation(self.naming(), &request.payee, &request.request_hash, 0);
        if let Some(read) = Box::pin(self.app.read_claim(&obligation)).await? {
            refuse_on_claim(&read.view)?;
        }
        let record_key = self.activity_key(&target)?;
        self.local
            .put(
                &key::reply(&record_key),
                &OwedReply {
                    payee: request.payee,
                    request_hash: request.request_hash,
                    cancel,
                    reason,
                    label: label.as_ref().map(|l| l.as_str().to_owned()),
                },
            )
            .await?;
        // The record write that fences attempts: conditioned on the claim at the step read.
        let mut fenced = record.clone();
        fenced.status = Some(if cancel { "cancelled" } else { "rejected" }.into());
        let condition = self.app.claim_condition(&obligation).await?;
        let value = fenced.to_json()?;
        if let Err(error) = Box::pin(self.app.put_record(
            &target.record_target(),
            &value,
            vec![condition],
        ))
        .await
        {
            self.local.delete(&key::reply(&record_key)).await?;
            if let Some(view) = claim_conflict(&error) {
                refuse_on_claim(&view)?;
            }
            return Err(crate::records::conflict(error));
        }
        self.send_reply(&record_key).await
    }

    /// Send the rejections and cancellations recorded but not yet sent.
    pub(crate) async fn send_owed_replies(&mut self) -> Result<()> {
        for (rest, _) in self.local.scan::<OwedReply>(key::REPLIES).await? {
            let Ok(record_key) = pubky_switch_core::enc::unb64_array::<32>(&rest) else {
                self.local
                    .delete(&format!("{}{rest}", key::REPLIES))
                    .await?;
                continue;
            };
            self.send_reply(&record_key).await?;
        }
        Ok(())
    }

    async fn send_reply(&mut self, record_key: &[u8; 32]) -> Result<String> {
        let owed: OwedReply = self
            .local
            .get(&key::reply(record_key))
            .await?
            .ok_or_else(PaykitError::corrupt)?;
        let target =
            ActivityTarget::payer(natural_key::request_record(&owed.payee, &owed.request_hash));
        let label = owed.label.as_deref().and_then(|l| Label::parse(l).ok());
        let reply = RequestReply {
            reason: owed.reason.clone(),
            label,
        };
        let (kind, role) = if owed.cancel {
            (Kind::PaymentRequestCancellation, Role::Cancellation)
        } else {
            (Kind::PaymentRequestRejection, Role::Response)
        };
        let msg_id = self
            .queue_message(
                owed.payee,
                kind,
                vec![owed.request_hash],
                reply.to_body(),
                Some((&target, role)),
            )
            .await?;
        let mut tx = Tx::default();
        if owed.cancel {
            tx.put(&key::side(&msg_id), &SideTag::Payer)?;
        }
        tx.delete(&key::reply(record_key));
        self.local.commit(tx).await?;
        Box::pin(self.app.flush_outbox()).await?;
        Ok(msg_id)
    }

    /// The acceptance of a one-time request, sent with its payment (Paykit spec 7.2 step 2,
    /// Paykit wire 4.6).
    pub(crate) async fn queue_acceptance(
        &mut self,
        payee: [u8; 32],
        request_hash: [u8; 32],
        label: Option<Label>,
    ) -> Result<String> {
        let target = ActivityTarget::payer(natural_key::request_record(&payee, &request_hash));
        let body = PaymentRequestAcceptance { label }.to_body();
        self.queue_message(
            payee,
            Kind::PaymentRequestAcceptance,
            vec![request_hash],
            body,
            Some((&target, Role::Response)),
        )
        .await
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
