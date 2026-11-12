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
use pubky_switch_client::app::{
    claim_conflict, AppError, Delivery, KeptReason, OpBatch, RecordContent, RecordRead,
    UndeliverableReason,
};
use pubky_switch_client::error::ClientError;
use pubky_switch_core::claims::{ClaimStatus, Outcome};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::Condition;
use serde_json::{Map, Value};

use crate::activity::{self, ActivityTarget, KeptSide, Role, SideTag};
use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::rail::ProofCheck;
use crate::receipts::{received_receipt, ReceiptDetails, ReceivedReceipt};
use crate::receive::KEPT_MESSAGE_MS;
use crate::records::conflict;
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

/// A one-time request to send (Paykit wire 4.5). Its `Debug` shows neither the payment
/// reference nor the metadata, and no endpoint payload.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
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

impl std::fmt::Debug for RequestDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestDraft")
            .field("amount", &self.amount)
            .field("payment_reference", &self.payment_reference)
            .field("proposal_expires_at_ms", &self.proposal_expires_at_ms)
            .field("payment_deadline_ms", &self.payment_deadline_ms)
            .field("destination", &self.destination)
            .field(
                "accepted_endpoint_identifiers",
                &self.accepted_endpoint_identifiers,
            )
            .field(
                "metadata",
                &self
                    .metadata
                    .as_ref()
                    .map(|m| format!("<{} members>", m.len())),
            )
            .finish()
    }
}

impl RequestDraft {
    /// A request for `amount`, with the reference copied into the payment where the method
    /// allows it, paid to `destination`. It has no expiry, no payment deadline, no limit on
    /// the identifiers the payer may use and no metadata until a `with_` setter adds one.
    pub fn new(
        amount: PaymentAmount,
        payment_reference: PaymentReference,
        destination: RequestDestination,
    ) -> Self {
        Self {
            amount,
            payment_reference,
            proposal_expires_at_ms: None,
            payment_deadline_ms: None,
            destination,
            accepted_endpoint_identifiers: None,
            metadata: None,
        }
    }

    /// Until when the payer may pay it; no expiry when not set.
    #[must_use]
    pub fn with_proposal_expires_at_ms(mut self, expires_at_ms: u64) -> Self {
        self.proposal_expires_at_ms = Some(expires_at_ms);
        self
    }

    /// The payment deadline; none when not set.
    #[must_use]
    pub fn with_payment_deadline_ms(mut self, deadline_ms: u64) -> Self {
        self.payment_deadline_ms = Some(deadline_ms);
        self
    }

    /// The only identifiers the payer may use.
    #[must_use]
    pub fn with_accepted_endpoint_identifiers(
        mut self,
        identifiers: Vec<PaymentEndpointIdentifier>,
    ) -> Self {
        self.accepted_endpoint_identifiers = Some(identifiers);
        self
    }

    /// Opaque metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Map<String, Value>) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Where a message this app sent is, such as a request, a proof or an acceptance, as the
/// user sees it (Switch spec 11).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MessageDelivery {
    /// The batch that records and queues it was signed and kept, the switch not answering
    /// now: it is sent again, the same bytes, at the app's next write or read, and the
    /// message is queued once it is answered (Switch wire 6.4 step 5). Calling again would
    /// send a second message.
    Kept {
        /// The kept batch's ID.
        batch_id: [u8; 16],
    },
    /// In the outbox, to be tried again.
    Queued {
        /// How many times it was tried.
        tries: u64,
        /// The last failure, without content.
        last_error: Option<String>,
    },
    /// "Waiting for them to admit you": the recipient's switch answered `403 not_admitted`,
    /// first at `since_ms` (app clock), as a payee's does to a payer it doesn't admit yet,
    /// or a payer's to a payee it hasn't invited. The message stays queued, and goes once
    /// the recipient adds this identity as a contact or invites it (Switch spec 7), or is
    /// given up after [`pubky_switch_client::app::UNDELIVERABLE_MS`] (Switch spec 11).
    WaitingForAdmission {
        /// The first such answer, app clock, in Unix milliseconds.
        since_ms: u64,
        /// How many times it was tried.
        tries: u64,
    },
    /// This app's own certificate isn't current: a delivery was answered `403 not_current`,
    /// first at `since_ms` (app clock). Nothing about the recipient: the message stays
    /// queued until a renewal makes the certificate current again (Switch wire 3.8), or is given up
    /// after [`pubky_switch_client::app::UNDELIVERABLE_MS`] (Switch spec 11).
    CertificateNotCurrent {
        /// The first such answer, app clock, in Unix milliseconds.
        since_ms: u64,
        /// How many times it was tried.
        tries: u64,
    },
    /// The recipient's inbox accepted it.
    Accepted {
        /// The acceptance time.
        accepted_at_ms: u64,
    },
    /// Refused for good, given up, or never queued: the kept batch that would have queued
    /// it was dropped. A batch dropped after a send whose answer can't tell whether it was
    /// applied may have been applied all the same: the request's record may then be at the
    /// switch although the message was never sent, and the host reads the request's state
    /// ([`Paykit::payee_activity`]) before it sends it again.
    Failed {
        /// Why, without content.
        reason: String,
        /// When, app clock: when it was refused or given up, or its batch dropped.
        failed_at_ms: u64,
    },
    /// A state of the client's this version of the SDK doesn't name, as the client reports
    /// it.
    Other(Delivery),
}

impl MessageDelivery {
    /// The client's [`Delivery`] as the user sees it. The last answer that made a delivery
    /// wait names which wait it is: the payee not admitting this identity, or this app's
    /// certificate not current.
    pub(crate) fn of(delivery: Delivery) -> Self {
        match delivery {
            Delivery::Queued {
                tries,
                last_error,
                undeliverable_since_ms,
                undeliverable_reason,
                ..
            } => match (undeliverable_since_ms, undeliverable_reason) {
                (Some(since_ms), Some(UndeliverableReason::NotAdmitted)) => {
                    Self::WaitingForAdmission { since_ms, tries }
                }
                (Some(since_ms), Some(UndeliverableReason::NotCurrent)) => {
                    Self::CertificateNotCurrent { since_ms, tries }
                }
                _ => Self::Queued { tries, last_error },
            },
            Delivery::Accepted { accepted_at_ms, .. } => Self::Accepted { accepted_at_ms },
            Delivery::Failed {
                reason,
                failed_at_ms,
                ..
            } => Self::Failed {
                reason,
                failed_at_ms,
            },
            Delivery::Kept { batch_id, .. } => Self::Kept { batch_id },
            // A state a later client adds, which this SDK doesn't name.
            other => Self::Other(other),
        }
    }
}

/// A request this app sent.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SentRequest {
    /// The message ID.
    pub msg_id: String,
    /// Where the message is.
    pub delivery: MessageDelivery,
    /// The request: its record is written in the batch that queues it, so it is known once
    /// the call returns, and written once that batch is answered.
    pub request: Option<RequestRef>,
}

/// What the payer sees for a request (Paykit spec 8.5).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
    /// Held after a restore or a `missed` item: the payee may have cancelled it since the
    /// backup, or among the entries the service deleted, and it is paid only once the user
    /// has seen that (Paykit spec 7.2, [`crate::PayOptions::possibly_cancelled_confirmed`],
    /// [`crate::Paykit::confirm_possibly_cancelled`]).
    Held,
    /// Received, but its record couldn't be written: the switch refused the write for good,
    /// such as over a quota (Switch spec 8.2). It is shown from the app's own copy and
    /// written into its record by a later write as room frees; it can't be paid, rejected or
    /// cancelled until then ([`Refusal::NotFound`]).
    Unrecorded,
    /// From a payee the identity's deny list holds, as the app last read it: paused, and
    /// paid only once the user confirms (Paykit spec 8.1, [`Refusal::Blocked`]).
    Blocked,
}

/// A request the payer holds. Its `Debug` shows the payment reference's length only.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The payee's receipt the record holds, if any (Paykit wire 4.11), as this app's copy
    /// shows it: for display. While the records aren't listed, after a restore or a
    /// certificate that reads more, the copy may lack a receipt the switch holds;
    /// [`Paykit::receipts`] is refused as not now meanwhile.
    pub receipt: Option<ReceivedReceipt>,
}

impl std::fmt::Debug for RequestView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestView")
            .field("request", &self.request)
            .field("record", &self.record)
            .field("amount", &self.amount)
            .field(
                "payment_reference",
                &format_args!("<{} chars>", self.payment_reference.chars().count()),
            )
            .field("label", &self.label)
            .field("proposal_expires_at_ms", &self.proposal_expires_at_ms)
            .field("recurring", &self.recurring)
            .field("status", &self.status)
            .field("receipt", &self.receipt)
            .finish()
    }
}

/// What the payee sees of a request it sent or a payment it received.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
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

/// The record holds this identity's acceptance (Paykit spec 8.1).
pub(crate) fn accepted(record: &ActivityRecord) -> bool {
    matches!(
        record.response.as_ref().map(StoredMessage::body),
        Some(Body::PaymentRequestAcceptance(_))
    )
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
    /// client's errors. A failure that leaves the request's batch kept (the network, `429`,
    /// `503`) is no error: the request is reported sent, [`MessageDelivery::Kept`], recorded
    /// and queued by the app's next write or read, so the host doesn't send a second one.
    /// After any other error but a store failure nothing of the request was kept, and the
    /// host may send it again. A store failure is returned bare when the client can't tell
    /// whether it kept the batch, which may still go out with the next write or read: a
    /// host that retries after one reads [`Paykit::payee_activity`] after its next write
    /// first.
    pub async fn send_request(&mut self, to: [u8; 32], draft: RequestDraft) -> Result<SentRequest> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.send_request_inner(to, draft)).await
    }

    async fn send_request_inner(
        &mut self,
        to: [u8; 32],
        draft: RequestDraft,
    ) -> Result<SentRequest> {
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
        let request = Some(RequestRef {
            payee: own,
            request_hash: hash,
        });
        if let Err(error) = self.app.submit_batch(batch).await {
            let Some(batch_id) = error.kept_batch() else {
                return Err(error.into());
            };
            // The client indexes the request by its `msg_id` in the commit that keeps the
            // batch, so `Paykit::message_delivery` answers it from there, after a reload too.
            return Ok(SentRequest {
                msg_id,
                delivery: MessageDelivery::Kept { batch_id },
                request,
            });
        }
        // The request is recorded and queued: a try to deliver it now that fails leaves it
        // queued for the next flush, and the call still reports it sent, so the caller
        // doesn't send a second request.
        let delivery = match self.app.flush_message(&msg_id).await {
            Ok(Some(delivery)) => delivery,
            _ => self
                .app
                .delivery(&msg_id)
                .await?
                .ok_or_else(PaykitError::corrupt)?,
        };
        Ok(SentRequest {
            msg_id,
            delivery: MessageDelivery::of(delivery),
            request,
        })
    }

    /// Where the message this app sent as `msg_id` is now, such as a request after the
    /// flushes since [`Paykit::send_request`], or a payment's proof: for example "waiting for
    /// them to admit you" ([`MessageDelivery::WaitingForAdmission`]), as the client answers
    /// it. A request whose batch the client still keeps is [`MessageDelivery::Kept`]; one
    /// whose kept batch it dropped is [`MessageDelivery::Failed`], with the drop's time,
    /// for [`pubky_switch_client::app::SENT_RETENTION_MS`] from that time, whether or not
    /// the host acknowledged the client's event about it.
    ///
    /// `None` is a message the client keeps nothing of: one accepted or ended longer ago
    /// than that retention, or queued before a restore. It never says the message was never
    /// sent, nor that sending it again is safe: the host reads the state it would change
    /// first, such as [`Paykit::payee_activity`] for a request.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn message_delivery(&self, msg_id: &str) -> Result<Option<MessageDelivery>> {
        Ok(self.app.delivery(msg_id).await?.map(MessageDelivery::of))
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
        self.refuse_while_unlisted().await?;
        let mut out = Vec::new();
        for held in self
            .app
            .records_of(Collection::PayeeActivity.name())
            .await?
        {
            match self.payee_record(&held.key).await {
                Ok((target, record)) => {
                    // A period record holds no request: its request record does, under the
                    // same label (Paykit wire 5.3).
                    let label = target.label();
                    let request = match crate::receipts::period_request_target(
                        &record,
                        &self.identity(),
                        label.as_ref(),
                    ) {
                        Some(of) => self.held_activity(&of).await?,
                        None => None,
                    };
                    out.push(payee_view(held.key, &record, request.as_ref()));
                }
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
    /// [`Refusal::CertificateRefused`] while the app's certificate fails Paykit's checks;
    /// [`Refusal::NotFound`] for a record without a proof, or a Billing Period's record:
    /// this SDK verifies the proof of a one-time request or of a payment without a request,
    /// and a period's amount and destination come with its period details, which recurring
    /// payments read; the rail's errors.
    pub async fn verify_payment(&mut self, record: &[u8; 32]) -> Result<Option<u64>> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.verify_payment_inner(record)).await
    }

    async fn verify_payment_inner(&mut self, record: &[u8; 32]) -> Result<Option<u64>> {
        self.refresh_own()?;
        self.records_known().await?;
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
        self.confirm_payment_with(record, ReceiptDetails::new())
            .await
    }

    /// [`Paykit::confirm_payment`], the receipt carrying `details`: the payee's metadata
    /// and, for a payment without a request, its reference for it (Paykit spec 5, Paykit
    /// wire 4.11). A receipt already sent is not sent again, whatever `details` say.
    ///
    /// # Errors
    ///
    /// As [`Paykit::confirm_payment`]; [`PaykitError::Lib`] for a `payment_reference` on a
    /// receipt of a request, whose reference applies, with nothing sent.
    pub async fn confirm_payment_with(
        &mut self,
        record: &[u8; 32],
        details: ReceiptDetails,
    ) -> Result<Option<String>> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.confirm_payment_with_inner(record, details)).await
    }

    async fn confirm_payment_with_inner(
        &mut self,
        record: &[u8; 32],
        details: ReceiptDetails,
    ) -> Result<Option<String>> {
        self.refresh_own()?;
        self.records_known().await?;
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
                metadata: details.metadata.clone(),
                payment_reference: details.payment_reference.clone(),
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
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.cancel_sent_request_inner(record, reason)).await
    }

    async fn cancel_sent_request_inner(
        &mut self,
        record: &[u8; 32],
        reason: Option<String>,
    ) -> Result<String> {
        self.refresh_own()?;
        self.records_known().await?;
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
            tx.put(
                &key::side(signed.msg_id()),
                &KeptSide {
                    msg_id: signed.msg_id().to_owned(),
                    side: SideTag::Payee,
                },
            )?;
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
    ///
    /// A record held that the app can't open, sealed under a scope key it doesn't hold yet
    /// or a value that doesn't read, is refused as not now, never as no request: it may hold
    /// a response or a cancellation, so no payment and no response is decided from it
    /// (Paykit spec 8.1, one response per request).
    pub(crate) async fn payer_request(
        &mut self,
        request: &RequestRef,
    ) -> Result<(ActivityTarget, ActivityRecord)> {
        let target = ActivityTarget::payer(natural_key::request_record(
            &request.payee,
            &request.request_hash,
        ));
        let Some(record) = self.activity(&target).await? else {
            let content = self
                .app
                .shared_record(&self.activity_key(&target)?)
                .await?
                .map(|held| held.content);
            return Err(match content {
                Some(content) if crate::hold::awaits_key(&content) => {
                    crate::hold::value_awaits_key()
                }
                Some(RecordContent::Value(_) | RecordContent::Unreadable(_)) => {
                    Refusal::NotNow("Paykit wire 5.3: the request record held doesn't read".into())
                        .into()
                }
                _ => Refusal::NotFound.into(),
            });
        };
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
        let Some(cancellation) = self.kept_stop_message(target).await? else {
            return Ok(false);
        };
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
        Ok(self.kept_stop_message(target).await?.is_some())
    }

    /// The payee's cancellation kept as the stop for the request record at `target`, read
    /// with the check a restore makes ([`crate::protocol::kept_cancellation_at`]): one that
    /// fails it is a corrupt store, never no stop.
    async fn kept_stop_message(&self, target: &ActivityTarget) -> Result<Option<StoredMessage>> {
        let kept_key = key::kept_cancellation(&self.activity_key(target)?);
        match self.app.protocol_get(kept_key.as_bytes()).await? {
            None => Ok(None),
            Some(value) => self.checked_stop(kept_key.as_bytes(), &value).map(Some),
        }
    }

    /// A stop's value `value` at `key`, checked as [`Paykit::kept_stop_message`] checks it.
    fn checked_stop(&self, key: &[u8], value: &[u8]) -> Result<StoredMessage> {
        crate::protocol::kept_cancellation_at(key, value, &self.identity(), &|target| {
            self.app
                .record_key(target)
                .map_err(|error| error.to_string())
        })
        .map_err(|_| PaykitError::corrupt())
    }

    /// Place every kept cancellation whose request record this app holds, each as a later
    /// write of its own with the kept copy dropped (Paykit spec 8.1). Run after each
    /// receive, at a resume, and before any payment. One that can't be placed now stays
    /// kept, for the next run; its kept copy refuses every new attempt meanwhile. A stop
    /// for a request the copy doesn't hold ([`crate::receive::holds_record`]) goes once
    /// [`KEPT_MESSAGE_MS`] have passed since the cancellation's acceptance, on the highest
    /// service time verified, but never while the copy of the payer's records may lack
    /// records, when the absence decides nothing, nor while the app keeps a stored message
    /// of that request to write ([`crate::stored::writes_request`]): the request's own entry
    /// may be one, waiting for a write that failed or for a listing, and written without
    /// the stop the request would be payable.
    pub(crate) async fn place_kept_cancellations(&mut self) -> Result<()> {
        for (stop_key, value) in self
            .app
            .protocol_scan(key::KEPT_CANCELLATIONS.as_bytes())
            .await?
        {
            // Read with a restore's check: a stop that fails it is a corrupt store, so the
            // receive fails rather than drop or place it.
            let cancellation = self.checked_stop(&stop_key, &value)?;
            let Some(request) = cancellation.message().refs.first().copied() else {
                return Err(PaykitError::corrupt());
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
                continue;
            }
            let content = self
                .app
                .shared_record(&self.activity_key(&target)?)
                .await?
                .map(|record| record.content);
            // A kept stop always has its acceptance time (the check refuses one without).
            let stale = cancellation.accepted_at().is_some_and(|at| {
                self.service_time()
                    .is_some_and(|t| at.saturating_add(KEPT_MESSAGE_MS) <= t)
            });
            if stale
                && !crate::receive::holds_record(content.as_ref())
                && !self.side_unlisted(crate::hold::RecordSide::Payer).await?
                && !self
                    .keeps_stored_of(&cancellation.sender(), &request)
                    .await?
            {
                let stop_key = String::from_utf8(stop_key).map_err(|_| PaykitError::corrupt())?;
                self.local_delete(&stop_key).await?;
            }
        }
        Ok(())
    }

    /// Whether the app keeps a stored message from `sender` of `request` still to write
    /// ([`crate::stored::writes_request`]).
    async fn keeps_stored_of(&self, sender: &[u8; 32], request: &[u8; 32]) -> Result<bool> {
        let own = self.identity();
        for (note_key, value) in self
            .app
            .protocol_scan(key::stored_notes_of(sender).as_bytes())
            .await?
        {
            if crate::stored::writes_request(&note_key, &value, &own, request)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every request the payer's apps received, from the `payer_activity` records this app
    /// holds, with what the user sees for each (Paykit spec 8.1 step 2, 8.5).
    ///
    /// The status is as of the app's last read: it comes from the records and the claim
    /// views this app holds and the highest service time it verified, with no network
    /// read. In order: paid, by the record or a SETTLED claim; rejected or cancelled, by the
    /// record; then the claim's open attempt or doubt; then the expiry. [`Paykit::receive`]
    /// brings it up to date. A request kept to write into its record later shows as
    /// [`RequestStatus::Unrecorded`]. It is for display: while the records aren't listed,
    /// after a restore or a certificate that reads more, the copy may lack records the
    /// switch holds; a payment of a request reads its record again first.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn requests(&self) -> Result<Vec<RequestView>> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.requests_inner()).await
    }

    async fn requests_inner(&self) -> Result<Vec<RequestView>> {
        self.refuse_while_unlisted().await?;
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
        for view in self.unrecorded_requests().await? {
            if !out.iter().any(|held| held.request == view.request) {
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
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.request_inner(request)).await
    }

    async fn request_inner(&self, request: &RequestRef) -> Result<RequestView> {
        self.refuse_while_unlisted().await?;
        let target = ActivityTarget::payer(natural_key::request_record(
            &request.payee,
            &request.request_hash,
        ));
        let Some(record) = self.held_activity(&target).await? else {
            return self
                .unrecorded_requests()
                .await?
                .into_iter()
                .find(|view| view.request == *request)
                .ok_or_else(|| Refusal::NotFound.into());
        };
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
        let stop = key::kept_cancellation(&record_key);
        let stopped = match self.app.protocol_get(stop.as_bytes()).await? {
            Some(value) => {
                self.checked_stop(stop.as_bytes(), &value)?;
                true
            }
            None => false,
        };
        if status == RequestStatus::Open && stopped {
            status = RequestStatus::Cancelled;
        }
        if status == RequestStatus::Open && self.held(&record_key, record).await? {
            status = RequestStatus::Held;
        }
        if status == RequestStatus::Open && self.denied_as_last_read(&payee).await? {
            status = RequestStatus::Blocked;
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
            receipt: received_receipt(record_key, record, Some(request)),
        }))
    }

    /// Whether the highest service time verified is at or past `deadline`.
    fn reached(&self, deadline: Option<u64>) -> bool {
        deadline.is_some_and(|deadline| self.service_time().is_some_and(|t| deadline <= t))
    }

    /// Whether the request in `record` may still be answered (Paykit spec 8.1, 8.2): not
    /// paid, rejected or cancelled, nor `stopped` by a cancellation the app keeps; a recurring
    /// one only rejected, and only before it is accepted, its cancellation and anything after
    /// its acceptance belonging to the recurring flows. Its expiry is judged apart, on the
    /// record read again ([`Paykit::expired_answer`]).
    fn refuse_answer(
        &self,
        record: &ActivityRecord,
        body: &PaymentRequest,
        cancel: bool,
        stopped: bool,
    ) -> Result<()> {
        if record.shows_paid() {
            return Err(Refusal::AlreadyPaid.into());
        }
        if rejected(record) {
            return Err(Refusal::Rejected.into());
        }
        if cancelled(record) || stopped {
            return Err(Refusal::Cancelled.into());
        }
        let recurring = body.is_recurring();
        if recurring && (cancel || record.response.is_some()) {
            return Err(Refusal::Recurring.into());
        }
        // One response per request (Paykit spec 8.1, "Rejecting and cancelling"): after an
        // acceptance the payer may cancel, after a rejection it neither accepts nor cancels,
        // and after a cancellation it sends no response. A one-time request accepted, by this
        // app or another own app whose acceptance the record holds, is cancelled, not
        // rejected, the acceptance's payment having failed included: the claim then shows no
        // open, `unknown` or paid attempt, so the cancellation goes.
        if !cancel && accepted(record) {
            return Err(Refusal::Usage("accepted; cancel instead".into()).into());
        }
        Ok(())
    }

    /// The request record at `target` as the switch holds it now, read again (Switch wire
    /// 7.4.9): only a write the read shows current is evidence, never a copy the app keeps
    /// from before. A value is held only when it reads and is bound to the key it was read
    /// at, whose natural key the request derives, so it holds that same request (Paykit wire
    /// 5.3). The switch holding none (`404`) is [`Fresh::Missing`]; a newer write the app
    /// refused, or one no newer than what it holds, is [`Fresh::Unreadable`]: the copy kept
    /// then shows nothing of the request now.
    ///
    /// # Errors
    ///
    /// A read that fails: [`Refusal::SwitchUnavailable`] for a switch that couldn't be read
    /// now, the switch's refusal for one it refused; never taken as the record unchanged.
    pub(crate) async fn fresh_request(&mut self, target: &ActivityTarget) -> Result<Fresh> {
        let record = match self.app.read_record_fresh(&target.record_target()).await? {
            RecordRead::Fresh(record) => record,
            RecordRead::Kept { reason, .. } => return fresh_kept(reason),
            // A read a later client names: nothing shows the switch's write held now.
            _ => return Ok(Fresh::Unreadable),
        };
        Ok(match record.content {
            RecordContent::Deleted => Fresh::Missing,
            RecordContent::Value(_) => match self.held_activity(target).await? {
                Some(record) => Fresh::Held(Box::new(record)),
                None => Fresh::Unreadable,
            },
            _ => Fresh::Unreadable,
        })
    }

    /// The refusal of an answer to a recurring request whose proposal expiry the highest
    /// service time verified has passed (Paykit spec 8.2), decided on the request record read
    /// again, never on a copy held: another own app may have accepted, rejected or
    /// cancelled the request before its expiry, and an accepted recurring request no longer
    /// expires as a whole. [`Refusal::DeadlinePassed`] only for a record that is there,
    /// reads, is bound to this request's key, holds this request, and leaves it unanswered
    /// and past its expiry; a closed one gives what closed it. Any other record gives
    /// `otherwise`, the error the answer met, or, without one, [`Refusal::NotFound`] for none
    /// and [`PaykitError::Conflict`] for one that doesn't read as this request.
    async fn expired_answer(
        &mut self,
        target: &ActivityTarget,
        cancel: bool,
        otherwise: Option<PaykitError>,
    ) -> Result<PaykitError> {
        Ok(match self.fresh_request(target).await? {
            Fresh::Held(fresh) => {
                let Some((_, body)) = request_of(&fresh) else {
                    return Ok(otherwise.unwrap_or_else(unreadable_request));
                };
                if let Err(closed) = self.refuse_answer(&fresh, body, cancel, false) {
                    closed
                } else if self.reached(expiry_deadline(&fresh, body)) {
                    Refusal::DeadlinePassed.into()
                } else {
                    otherwise.unwrap_or_else(conflict_changed)
                }
            }
            Fresh::Missing => otherwise.unwrap_or_else(|| Refusal::NotFound.into()),
            Fresh::Unreadable => otherwise.unwrap_or_else(unreadable_request),
        })
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
        let view = self
            .app
            .claim_evidence(&obligation)
            .await?
            .map(|evidence| evidence.view);
        // A payment made counts, a cancellation it crossed included (Paykit spec 8.1).
        if view
            .as_ref()
            .is_some_and(|view| view.status == ClaimStatus::Settled)
        {
            return Ok(RequestStatus::Paid);
        }
        // The record's answers come before the claim's open attempt: a claim view kept from
        // before can still show an attempt the record's rejection or cancellation followed.
        if rejected(record) {
            return Ok(RequestStatus::Rejected);
        }
        if cancelled(record) {
            return Ok(RequestStatus::Cancelled);
        }
        if let Some(view) = view {
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
        // Expired once the deadline is at or before the highest service time verified (Paykit
        // spec 7.1): a one-time request's attempt deadline, or a recurring request's
        // proposal expiry while it isn't accepted (Paykit spec 8.2; v0.1's
        // `proposal_expired`).
        if self.reached(expiry_deadline(record, request)) {
            return Ok(RequestStatus::Expired);
        }
        Ok(RequestStatus::Open)
    }

    /// Reject a request (Paykit spec 8.1, Paykit wire 4.7): recorded first, conditioned on
    /// its claim having no open or paid attempt, then sent. Returns the rejection's `msg_id`.
    /// A recurring request is rejected before its acceptance, conditioned on its proposal
    /// not having expired, as its acceptance would be (Paykit spec 8.2), so a wallet that
    /// doesn't pay recurring requests can still dismiss one.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`]; [`Refusal::Rejected`], [`Refusal::Cancelled`],
    /// [`Refusal::AlreadyPaid`] or [`Refusal::PaymentInProgress`]; [`Refusal::Recurring`] for
    /// a recurring request already accepted; [`Refusal::DeadlinePassed`] for a recurring
    /// request whose proposal expired, decided once the record the service holds shows it
    /// still unanswered: one another own app accepted meanwhile gives [`Refusal::Recurring`],
    /// one rejected or cancelled gives that refusal; [`Refusal::Usage`] for a one-time request
    /// the record shows accepted, by this app or another own app, which is cancelled instead,
    /// also once the acceptance's payment failed (Paykit spec 8.1, one response per request);
    /// [`PaykitError::Conflict`] when the record changed meanwhile; the client's errors.
    pub async fn reject_request(
        &mut self,
        request: &RequestRef,
        reason: Option<String>,
    ) -> Result<String> {
        self.refresh_own()?;
        self.records_known().await?;
        // The answer's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.answer_request(request, false, reason)).await
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
        self.records_known().await?;
        // The answer's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.answer_request(request, true, reason)).await
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
        let recurring = body.is_recurring();
        let label = body.label.clone();
        let proposal_expires_at = body.proposal_expires_at;
        let stopped = self.kept_stop(&target).await?;
        self.refuse_answer(&record, body, cancel, stopped)?;
        let expiry = expiry_deadline(&record, body);
        // Past its proposal expiry, a recurring request can't be answered: the batch's time
        // condition would refuse it on every try. The copy held may be stale, so the record
        // the switch holds decides.
        if recurring && self.reached(expiry) {
            return Err(self.expired_answer(&target, cancel, None).await?);
        }
        let obligation = self.obligation(&request.payee, &request.request_hash, 0);
        if !recurring {
            if let Some(read) = self.app.read_claim(&obligation).await? {
                refuse_on_claim(&read.view)?;
            }
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
            tx.put(
                &key::side(signed.msg_id()),
                &KeptSide {
                    msg_id: signed.msg_id().to_owned(),
                    side: SideTag::Payer,
                },
            )?;
        }
        // Recorded in the request record in the batch that queues it, on the version of the
        // record read. A one-time request's reply is also conditioned on the claim at the
        // step read: an attempt meanwhile fails the batch, and the reply goes nowhere (Paykit
        // spec 8.1). A recurring request has no claim before it is accepted; its rejection is
        // conditioned as its acceptance is, on the proposal not having expired, so an
        // acceptance and a rejection can't both go out (Paykit spec 8.2).
        let conditions = if recurring {
            proposal_expires_at
                .map(|expires_at| Condition::ServerTimeIn {
                    start: None,
                    end: Some(expires_at.saturating_add(1)),
                })
                .into_iter()
                .collect()
        } else {
            vec![self.app.claim_condition(&obligation).await?]
        };
        let msg_id = match self
            .send_with_record(signed, &target, role, conditions, tx.into_writes())
            .await
        {
            Ok(msg_id) => msg_id,
            Err(error) => {
                if let PaykitError::Client(client) = &error {
                    if let Some(view) = claim_conflict(client.unkept()) {
                        refuse_on_claim(&view)?;
                    }
                }
                // The proposal expired at the service: the answer's verified time says so.
                // Only on `409 conflict`: any other failure is the error. The record the
                // service holds decides, read again, and a record that isn't there or doesn't
                // read as this request leaves the conflict as the answer.
                if recurring && error.code() == Some(ErrorCode::Conflict) && self.reached(expiry) {
                    let error = conflict(error);
                    return Err(self.expired_answer(&target, cancel, Some(error)).await?);
                }
                return Err(conflict(error));
            }
        };
        self.app.flush_message(&msg_id).await?;
        Ok(msg_id)
    }
}

/// The request record the switch holds, read again ([`Paykit::fresh_request`]).
pub(crate) enum Fresh {
    /// It is there, reads, is bound to its key and holds the request.
    Held(Box<ActivityRecord>),
    /// There is none, or it was deleted.
    Missing,
    /// It doesn't read, isn't bound to its key, or holds another request; or the switch's
    /// write was refused, or is no newer than the one held, so nothing read now shows it.
    Unreadable,
}

/// What a read that kept the earlier copy says of the request record now
/// ([`Paykit::fresh_request`]).
fn fresh_kept(reason: KeptReason) -> Result<Fresh> {
    match reason {
        KeptReason::NotFound => Ok(Fresh::Missing),
        KeptReason::Unreachable(why) => Err(Refusal::SwitchUnavailable(why).into()),
        KeptReason::SwitchRefused { code } => {
            Err(PaykitError::Client(AppError::Switch(ClientError::Switch {
                status: code.status(),
                code,
                message: "the switch refused the record's read".into(),
                current: serde_json::Value::Null,
                known_version: None,
                retry_after: None,
            })))
        }
        // Refused, no newer than the write held, or a reason a later client names.
        _ => Ok(Fresh::Unreadable),
    }
}

/// The refusal of a request record that doesn't read as the request it should hold.
pub(crate) fn unreadable_request() -> PaykitError {
    PaykitError::Conflict("the request record the switch holds doesn't read as this request".into())
}

fn conflict_changed() -> PaykitError {
    PaykitError::Conflict("the record changed meanwhile".into())
}

/// The first service time at which `request` counts as expired: a one-time request's attempt
/// deadline (Paykit spec 7.1), or, for a recurring request not yet accepted, the millisecond
/// after its proposal expiry, which is inclusive (Paykit spec 8.2). `None` when it never
/// expires as a whole: no deadline, or a recurring request accepted, whose periods have
/// deadlines of their own.
fn expiry_deadline(record: &ActivityRecord, request: &PaymentRequest) -> Option<u64> {
    if request.is_recurring() {
        return record
            .response
            .is_none()
            .then_some(request.proposal_expires_at?)
            .map(|expires_at| expires_at.saturating_add(1));
    }
    request.attempt_deadline(None).ok().flatten()
}

/// A rejection or cancellation is refused while the claim has an open or paid attempt
/// (Paykit spec 8.1).
fn refuse_on_claim(view: &pubky_switch_core::claims::ClaimView) -> Result<()> {
    match view.status {
        ClaimStatus::Settled => Err(Refusal::AlreadyPaid.into()),
        ClaimStatus::Attempting => Err(Refusal::PaymentInProgress.into()),
        ClaimStatus::Free => {
            if view.attempts.iter().any(|a| a.outcome == Outcome::Unknown) {
                Err(Refusal::MayBePaid {
                    reason: "an attempt's outcome is unknown".into(),
                    doubt: None,
                }
                .into())
            } else {
                Ok(())
            }
        }
    }
}

/// The payee's view of `record`, held at `record_key`. `request` is the request record a
/// period record names, which holds the request a period record doesn't (Paykit wire 5.3).
fn payee_view(
    record_key: [u8; 32],
    record: &ActivityRecord,
    request: Option<&ActivityRecord>,
) -> PayeeActivity {
    let request = request_of(record).or_else(|| request.and_then(request_of));
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
        request_hash: request
            .map(|(stored, _)| stored.hash())
            .or(record.request_hash),
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

    /// Bob's payee request record of his request to alice, and a payee period record of it,
    /// as another own app wrote them: the period record holds no request.
    fn payee_period_records() -> (ActivityRecord, ActivityRecord) {
        use crate::testkit::{object, Party};
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let request = bob.message(
            &alice,
            "paykit.payment_request",
            vec![],
            object(json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": "invoice-7",
                "proposal_expires_at": null
            })),
        );
        let mut request_record =
            ActivityRecord::new(paykit_lib::records::ActivitySide::Payee, alice.id());
        request_record.request = Some(request.clone());
        let mut period_record =
            ActivityRecord::new(paykit_lib::records::ActivitySide::Payee, alice.id());
        period_record.request_hash = Some(request.hash());
        period_record.billing_period = Some(paykit_lib::periods::PeriodRef {
            starts_at: 1_000,
            ends_at: 2_000,
        });
        (request_record, period_record)
    }

    #[test]
    fn test_payee_view_of_a_period_shows_its_request() {
        let (request_record, period_record) = payee_period_records();
        let hash = period_record.request_hash.unwrap();
        let view = payee_view([9; 32], &period_record, Some(&request_record));
        assert_eq!(view.request_hash, Some(hash));
        assert_eq!(
            view.amount,
            request_of(&request_record).map(|(_, r)| r.amount.clone())
        );
    }

    fn queued(since: Option<u64>, reason: Option<UndeliverableReason>) -> Delivery {
        Delivery::Queued {
            tries: 2,
            last_error: Some("403".into()),
            undeliverable_since_ms: since,
            undeliverable_reason: reason,
        }
    }

    #[test]
    fn test_message_delivery_names_the_wait_for_admission() {
        assert_eq!(
            MessageDelivery::of(queued(Some(7), Some(UndeliverableReason::NotAdmitted))),
            MessageDelivery::WaitingForAdmission {
                since_ms: 7,
                tries: 2
            }
        );
    }

    #[test]
    fn test_message_delivery_keeps_a_certificate_not_current_apart() {
        assert_eq!(
            MessageDelivery::of(queued(Some(7), Some(UndeliverableReason::NotCurrent))),
            MessageDelivery::CertificateNotCurrent {
                since_ms: 7,
                tries: 2
            }
        );
        assert_eq!(
            MessageDelivery::of(queued(None, None)),
            MessageDelivery::Queued {
                tries: 2,
                last_error: Some("403".into())
            }
        );
    }

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
            Some(Refusal::MayBePaid { .. })
        ));
    }
}
