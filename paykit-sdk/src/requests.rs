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
    Conversion, PaymentDeadline, PaymentReceipt, PaymentRequest, PaymentRequestAcceptance,
    RequestDestination, RequestReply,
};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::periods::Recurrence;
use paykit_lib::records::{ActivityRecord, ActivitySide, RecordBinding, StoredMessage};
use paykit_lib::scopes::{PaykitScope, ScopeName};
use paykit_lib::{
    AttemptId, Body, Label, PaymentAmount, PaymentEndpointIdentifier, PaymentReference,
};
use pubky_switch_client::app::{
    claim_conflict, AppError, Delivery, FeedPosition, KeptReason, OpBatch, OutgoingMessage,
    RecordContent, RecordRead, UndeliverableReason,
};
use pubky_switch_client::error::ClientError;
use pubky_switch_core::claims::{ClaimStatus, Outcome};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::Condition;
use serde_json::{Map, Value};

use crate::activity::{self, ActivityTarget, KeptSide, Role, SideTag};
use crate::error::{PaykitError, Refusal, Result};
use crate::pay::{BlockedPayment, WriteStamp};
use crate::paykit::Paykit;
use crate::periods::PeriodView;
use crate::rail::ProofCheck;
use crate::receipts::{
    displayed_receipt, receipt_binding, received_receipt, ReceiptBinding, ReceiptDetails,
    ReceivedReceipt,
};
use crate::receive::KEPT_MESSAGE_MS;
use crate::records::{compare_received, conflict, first_received, KeptMessage, ReceivedAt};
use crate::settlements::{SettlementCredit, SettlementWrite};
use crate::state::{key, Tx};

/// How many times a payee's write is built again after its record changed meanwhile.
const WRITE_TRIES: usize = 3;

/// The proof and occurrence the payment method verified, with the signed Switch
/// revisions a receipt must still find when it is recorded and queued.
struct VerifiedCredit {
    paid_at: u64,
    target: ActivityTarget,
    activity: ActivityRecord,
    parent: Option<ActivityTarget>,
    conditions: Vec<Condition>,
    settlement: Option<SettlementWrite>,
}

/// A request, by its payee and its request hash (Paykit wire 1.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestRef {
    /// The payee identity.
    pub payee: [u8; 32],
    /// The request hash.
    pub request_hash: [u8; 32],
}

/// A request to send (Paykit wire 4.5): one-time, or recurring with a
/// [`RequestDraft::recurrence`]. Its `Debug` shows neither the payment reference nor the
/// metadata, endpoint payload or invitation code.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestDraft {
    /// The amount requested, for each Billing Period of a recurring request.
    pub amount: PaymentAmount,
    /// The reference copied into the payment where the method allows it.
    pub payment_reference: PaymentReference,
    /// Until when the payer may pay a one-time request, or accept a recurring one; `None`
    /// for no expiry.
    pub proposal_expires_at_ms: Option<u64>,
    /// A one-time request's payment deadline, `None` for none.
    pub payment_deadline_ms: Option<u64>,
    /// A recurring request's Billing Periods (Paykit spec 8.2); `None` for a one-time
    /// request.
    pub recurrence: Option<Recurrence>,
    /// A recurring request's payment deadline for each Billing Period, in seconds after the
    /// period's start; `None` for none.
    pub period_deadline_s: Option<u64>,
    /// With a recurrence: the request's endpoints serve only the first Billing Period, and
    /// each later one is paid only to the endpoints of its period details (Paykit spec 8.2).
    pub per_period_endpoints: bool,
    /// Where it is paid: endpoints of this app, an app of this identity, or any.
    pub destination: RequestDestination,
    /// The only identifiers the payer may use.
    pub accepted_endpoint_identifiers: Option<Vec<PaymentEndpointIdentifier>>,
    /// The conversion terms (Paykit wire 4.4): fixed rates pricing the endpoints of other
    /// assets, or `per_period` on a recurring request, each Billing Period's rates coming
    /// with its period details; `None` for none.
    pub conversion: Option<Conversion>,
    /// Opaque metadata.
    pub metadata: Option<Map<String, Value>>,
    /// An invitation to present when delivering this request to the payer's Switch
    /// (Switch spec 7). It is transport data, outside the signed payment request body,
    /// and grants no payment approval. `None` uses ordinary admission.
    pub invite_code: Option<String>,
}

impl std::fmt::Debug for RequestDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestDraft")
            .field("amount", &self.amount)
            .field("payment_reference", &self.payment_reference)
            .field("proposal_expires_at_ms", &self.proposal_expires_at_ms)
            .field("payment_deadline_ms", &self.payment_deadline_ms)
            .field("recurrence", &self.recurrence)
            .field("period_deadline_s", &self.period_deadline_s)
            .field("per_period_endpoints", &self.per_period_endpoints)
            .field("destination", &self.destination)
            .field(
                "accepted_endpoint_identifiers",
                &self.accepted_endpoint_identifiers,
            )
            .field("conversion", &self.conversion)
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
    /// A one-time request for `amount`, with the reference copied into the payment where the
    /// method allows it, paid to `destination`. It has no expiry, no payment deadline, no
    /// limit on the identifiers the payer may use, no recurrence, no metadata and no
    /// invitation until a `with_` setter adds one.
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
            recurrence: None,
            period_deadline_s: None,
            per_period_endpoints: false,
            destination,
            accepted_endpoint_identifiers: None,
            conversion: None,
            metadata: None,
            invite_code: None,
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

    /// Recurring, with `recurrence`'s Billing Periods (Paykit spec 8.2): the payer accepts
    /// it once, and pays `amount` for each period.
    #[must_use]
    pub fn with_recurrence(mut self, recurrence: Recurrence) -> Self {
        self.recurrence = Some(recurrence);
        self
    }

    /// A recurring request's payment deadline: each Billing Period is paid at the latest
    /// `seconds` after its start (Paykit wire 4.4).
    #[must_use]
    pub fn with_period_deadline_s(mut self, seconds: u64) -> Self {
        self.period_deadline_s = Some(seconds);
        self
    }

    /// A recurring request whose endpoints serve only the first Billing Period: each later
    /// one is paid to the endpoints of its period details (Paykit spec 8.2,
    /// [`Paykit::send_period_details`]).
    #[must_use]
    pub fn with_per_period_endpoints(mut self) -> Self {
        self.per_period_endpoints = true;
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

    /// The conversion terms (Paykit wire 4.4): with fixed rates, a payer pays an endpoint a
    /// rate prices the amount that rate gives, rounded up once to the endpoint's smallest
    /// unit, only once its user confirms it, and the payee credits that amount and no other
    /// (Paykit spec 7.2 step 1).
    #[must_use]
    pub fn with_conversion(mut self, conversion: Conversion) -> Self {
        self.conversion = Some(conversion);
        self
    }

    /// Opaque metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Map<String, Value>) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Present `code` when the Switch delivers this request. Queuing and retrying keep
    /// the same signed request; the code is not payment consent (Switch spec 7, 11).
    #[must_use]
    pub fn with_invite_code(mut self, code: String) -> Self {
        self.invite_code = Some(code);
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
    /// the recipient keeps this identity as a peer or invites it (Switch spec 7), or is
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

/// A message this app sent that its record holds, such as an acceptance or period details:
/// written in the batch that queues it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SentMessage {
    /// The message ID.
    pub msg_id: String,
    /// Where the message is. [`MessageDelivery::Kept`] when the batch that records and queues
    /// it is kept, to go at the app's next write or read: calling again would send a second
    /// message.
    pub delivery: MessageDelivery,
}

/// What the user confirmed before accepting a subscription ([`Paykit::accept_subscription_with`]),
/// as the guards of paying take it (Paykit spec 7.2, 8.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AcceptOptions {
    /// The user has seen that the payee may have cancelled this request since the backup the
    /// app was restored from, and accepts it anyway ([`Refusal::HeldAfterRestore`]; Paykit
    /// spec 7.2, "What a restore loses"). It ends the request's hold; it never overrides a
    /// cancellation the app holds. The value must be the exact hold shown: another restore
    /// or missed item requires a new confirmation.
    pub possibly_cancelled_confirmed: Option<crate::HeldRequest>,
    /// The subscription of a blocked payee the user was shown and chose to accept, as a
    /// [`Refusal::Blocked`] gave it (Paykit spec 8.1): only that same request is accepted.
    pub blocked_confirmed: Option<BlockedPayment>,
}

impl AcceptOptions {
    /// The user has seen that the payee may have cancelled this request since the backup the
    /// app was restored from, and accepts it anyway ([`Refusal::HeldAfterRestore`]).
    #[must_use]
    pub fn with_possibly_cancelled_confirmed(mut self, confirmed: crate::HeldRequest) -> Self {
        self.possibly_cancelled_confirmed = Some(confirmed);
        self
    }

    /// The subscription of a blocked payee the user was shown and chose to accept, as a
    /// [`Refusal::Blocked`] gave it: only that same request is accepted.
    #[must_use]
    pub fn with_blocked_confirmed(mut self, payment: BlockedPayment) -> Self {
        self.blocked_confirmed = Some(payment);
        self
    }
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
    /// An `unknown` or orphaned attempt, or a one-time request whose payer record lost its
    /// payment history to deletion (Paykit spec 6, 7.1).
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
    /// Its request record is divergent after a provider loss, or was seen so, and the user
    /// hasn't confirmed it: the payee may have cancelled it. Paid only once the user has
    /// seen that ([`Paykit::confirm_divergent`], Paykit spec 6).
    Divergent,
    /// A recurring request this identity accepted, the subscription running: each Billing
    /// Period is paid on its own (Paykit spec 8.2). Either side's cancellation ends it: the
    /// payer's ([`Paykit::cancel_request`]) is refused only while a listed period has an open
    /// or `unknown` attempt (Paykit spec 8.2, "Ending").
    Subscribed,
    /// An accepted recurring request whose last Billing Period has ended, on the highest
    /// service time verified: no period is left to come, though a payment of an earlier one
    /// may still arrive, as in v0.1. Only a request whose recurrence has an `ends_at` ends.
    Ended,
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
    /// A recurring request's Billing Periods (Paykit spec 8.2).
    pub recurrence: Option<Recurrence>,
    /// What the user sees.
    pub status: RequestStatus,
    /// The current restore or missed-item hold to show before confirmation. A later restore
    /// or missed item requires showing its new hold. A payer deletion marker makes payment
    /// permanently unavailable and carries no confirmation.
    pub held: Option<crate::HeldRequest>,
    /// Its request record is paused after a view showed it divergent, or its latest view
    /// shows it divergent (Paykit spec 6): shown whatever the status
    /// ([`Paykit::divergent_records`]).
    pub divergent: bool,
    /// The payee's receipt the record holds, if any (Paykit wire 4.11), as this app's copy
    /// shows it: for display. While the records aren't listed, after a restore or a
    /// certificate that reads more, the copy may lack a receipt the switch holds;
    /// [`Paykit::receipts`] is refused as not now meanwhile.
    pub receipt: Option<ReceivedReceipt>,
    /// The request is paid only by hand: its record holds `manual_only` (Paykit wire 5.3),
    /// which is never lifted (Paykit spec 8.3, "Manual only"). A one-time request that came
    /// back after the user's block is opened so (Paykit spec 8.1, "Blocked counterparties"):
    /// the host shows that it came from a sender the user had blocked. No app pays it
    /// automatically, a resume's repeat of a payment the user started included; the user
    /// pays it with [`Paykit::pay_request`].
    pub manual_only: bool,
    /// A recurring request's period records this app holds, in period order, as its copy
    /// shows them: for display.
    pub periods: Vec<PeriodView>,
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
            .field("recurrence", &self.recurrence)
            .field("status", &self.status)
            .field("held", &self.held)
            .field("divergent", &self.divergent)
            .field("receipt", &self.receipt)
            .field("manual_only", &self.manual_only)
            .field("periods", &self.periods)
            .finish()
    }
}

/// What the payee sees of a request it sent or a payment it received.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PayeeStatus {
    /// Sent; no answer yet.
    Requested,
    /// Accepted: the payer is paying, or, for a recurring request, has subscribed.
    Accepted,
    /// Rejected by the payer.
    Rejected,
    /// Cancelled by either side.
    Cancelled,
    /// A proof is held, not credited: the payee verifies it by its own method. One whose
    /// settlement credits another occurrence already stays so (Paykit spec 8.4).
    ProofReceived,
    /// Credited: the record holds the payee's receipt and the payer's proof it confirms.
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
    /// Its record is paused after a view showed it divergent, or its latest view shows it
    /// divergent (Paykit spec 6): a proof is credited only once the user confirms it
    /// ([`Paykit::confirm_divergent`]).
    pub divergent: bool,
    /// Whether the request is recurring.
    pub recurring: bool,
    /// A recurring request's period records this app holds, in period order, each with
    /// its details, proof and receipt.
    pub periods: Vec<PeriodView>,
}

/// Whether `stop`, a payee's cancellation kept as the stop for the payer's request record that
/// `record` reads as, stops that request (Paykit spec 3.4, 8.1): the one rule of every reader
/// of a kept stop, the attempts, the request's view, the period details' window and the later
/// write that places it. A stop from an app whose certificate holds neither `paykit_request`
/// nor the request's label stops nothing once the record reads, and that write drops it. While
/// the record is absent or doesn't read (`None`), the stop stands: a denial, never a payment.
pub(crate) fn stop_applies(stop: &StoredMessage, record: Option<&ActivityRecord>) -> bool {
    !record
        .and_then(request_of)
        .is_some_and(|(_, request)| stop.check_payee_sender(request).is_err())
}

/// Place the first authorized original only if its acceptance order is known.
fn first_kept_stop(
    copies: impl Iterator<Item = (ReceivedAt, StoredMessage)>,
    record: Option<&ActivityRecord>,
    order: impl Fn(FeedPosition, FeedPosition) -> Option<std::cmp::Ordering>,
) -> Result<Option<StoredMessage>> {
    let mut applicable: Vec<_> = copies
        .filter(|(_, stop)| stop_applies(stop, record))
        .collect();
    if applicable.is_empty() {
        return Ok(None);
    }
    let first = first_received(&applicable, |a, b| compare_received(a.0, b.0, &order)).ok_or_else(
        || {
            Refusal::NotNow(
                "the order of the request's cancellations is unknown after provider loss".into(),
            )
        },
    )?;
    Ok(Some(applicable.swap_remove(first).1))
}

/// The values a list read of the requests shares across its records, each read once when
/// first needed: the hold's own values, the copy of the deny list, and the block floor with
/// the raise owed. Nothing is written between the reads, as the list reads take `&self`.
#[derive(Default)]
pub(crate) struct ListReads {
    hold: crate::hold::HoldReads,
    denied: Option<std::collections::HashSet<[u8; 32]>>,
    floor: Option<(crate::floor::SharedFloor, Option<u64>)>,
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

/// A deleted payer record may have held the only proof of payment (Paykit spec 6).
pub(crate) fn payment_history_lost() -> Refusal {
    Refusal::RecordHeld(
        "Paykit spec 6: a deleted activity record may have held the payment's proof".into(),
    )
}

/// A payment stops for a proof, rejection or cancellation. An accepted attempt finishes
/// across its payee's cancellation of the parent request (Paykit spec 8.1).
pub(crate) fn payment_ended(record: &ActivityRecord, crossing_payee: Option<[u8; 32]>) -> bool {
    record.shows_paid()
        || rejected(record)
        || record
            .cancellation
            .as_ref()
            .is_some_and(|stop| Some(stop.sender()) != crossing_payee)
}

impl Paykit {
    // ---------------------------------------------------------------------------------
    // The payee

    /// The label this app sends requests under (Paykit spec 3.1): the configured one, which
    /// it must hold; for an app holding only labelled scopes, its one label when it holds
    /// exactly one.
    fn selling_label(&self) -> Result<Option<Label>> {
        let scopes = self.scopes();
        let plain = scopes.iter().any(|s| s == "paykit_request");
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

    /// Send a request to `to` (Paykit spec 8.1 step 1, 8.2; Paykit wire 4.5), under this
    /// app's selling label: one-time, or recurring with a recurrence. The payer must admit
    /// this identity, for example with [`RequestDraft::invite_code`] (Paykit spec 1).
    /// The request is signed, and
    /// its record written in the batch that queues it, then it is sent. Nothing is signed,
    /// written or queued while sends to `to` are held: at an app holding `paykit_contacts`, while
    /// the copy of the deny list holds it, or while that copy isn't known, which a read of
    /// the lists is tried for first (Paykit spec 8.1, "Blocked counterparties"). An app
    /// without `paykit_contacts` reads no lists, and holds nothing.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] while sends to `to` are held, with nothing recorded or queued;
    /// [`Refusal::ScopeMissing`] without `paykit_request` or a label; [`Refusal::Label`];
    /// [`PaykitError::Lib`] with `Validation` for a draft that breaks Paykit wire 4.5, such
    /// as a payment deadline of the other kind of request, or both kinds; the
    /// client's errors. A failure that leaves the request's batch kept (the network, `429`,
    /// `503`) is no error: the request is reported sent, [`MessageDelivery::Kept`], recorded
    /// and queued by the app's next write or read, so the host doesn't send a second one.
    /// After any other error but a store failure nothing of the request was kept, and the
    /// host may send it again. A store failure is returned bare when the client can't tell
    /// whether it kept the batch, which may still go out with the next write or read: a
    /// host that retries after one reads [`Paykit::payee_activity`] after its next write
    /// first.
    pub async fn send_request(&mut self, to: [u8; 32], draft: RequestDraft) -> Result<SentRequest> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.send_request_inner(to, draft)).await
    }

    async fn send_request_inner(
        &mut self,
        to: [u8; 32],
        draft: RequestDraft,
    ) -> Result<SentRequest> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.require_payee()?;
        // Nothing new starts with a counterparty whose sends are held (Paykit spec 8.1,
        // "Blocked counterparties"), refused before anything is signed, written or queued.
        self.refuse_while_sends_held(&to).await?;
        let label = self.selling_label()?;
        // A one-time request's deadline is a time, a recurring one's a delay after each
        // period's start (Paykit wire 4.4); the writer refuses the one that doesn't match.
        let payment_deadline = match (draft.payment_deadline_ms, draft.period_deadline_s) {
            (Some(_), Some(_)) => {
                return Err(paykit_lib::Error::Validation(
                    "Paykit wire 4.5: a request has one payment deadline".into(),
                )
                .into());
            }
            (Some(at), None) => Some(PaymentDeadline::At(at)),
            (None, Some(seconds)) => Some(PaymentDeadline::PeriodStart { seconds }),
            (None, None) => None,
        };
        let request = PaymentRequest {
            amount: draft.amount,
            payment_reference: draft.payment_reference,
            proposal_expires_at: draft.proposal_expires_at_ms,
            payment_deadline,
            recurrence: draft.recurrence,
            destination: draft.destination,
            per_period_endpoints: draft.per_period_endpoints,
            label: label.clone(),
            accepted_payment_endpoint_identifiers: draft.accepted_endpoint_identifiers,
            conversion: draft.conversion,
            metadata: draft.metadata,
        };
        // The writer reads back what it writes by the recipient's rules (Paykit wire 4.5).
        let body = request.to_body()?;
        let mut message = OutgoingMessage::new(to, Kind::PaymentRequest.as_str(), body);
        message.invite_code = draft.invite_code;
        let signed = self.app.sign_message(message)?;
        let msg_id = signed.msg_id().to_owned();
        let hash = signed.hash();
        // The payee's request record, from the signed request, in the batch that queues it
        // (Paykit spec 8.1 step 1): its key derives from the request's hash.
        let own = self.identity();
        let target =
            ActivityTarget::payee(natural_key::request_record(&own, &hash), label.as_ref());
        self.activity_write_base(&self.activity_key(&target)?)
            .await?;
        let mut record = activity::empty(ActivitySide::Payee, to);
        activity::place(&mut record, Role::Request, self.own_copy(&signed)?);
        let value = record.to_json()?;
        let write = self
            .app
            .record_op(&target.record_target(), Some(&value), Vec::new())
            .await?;
        let mut tx = Tx::default();
        self.note_peer(&mut tx, &to)?;
        // What ends its outbox copy past its 30 days, in the commit that queues it (Paykit
        // spec 8.1).
        tx.put(
            &key::queued_stop(&msg_id),
            &crate::send_holds::QueuedStop::signed(
                &signed,
                crate::send_holds::HeldKind::Request,
                Some(hash),
                Some(&target),
            )?,
        )?;
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(write)
            .queue(signed)
            .locals(tx.into_writes());
        let request = Some(RequestRef {
            payee: own,
            request_hash: hash,
        });
        if let Err(error) = self.submit(batch).await {
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
        Ok(SentRequest {
            delivery: self.delivered(&msg_id).await?,
            msg_id,
            request,
        })
    }

    /// Where `msg_id`, recorded and queued, is once a try to deliver it now is made: a try
    /// that fails leaves it queued for the next flush, and the call still reports it sent,
    /// so the caller doesn't send a second message.
    pub(crate) async fn delivered(&mut self, msg_id: &str) -> Result<MessageDelivery> {
        let delivery = match self.send_message(msg_id).await {
            Ok(Some(delivery)) => delivery,
            _ => self
                .app
                .delivery(msg_id)
                .await?
                .ok_or_else(PaykitError::corrupt)?,
        };
        Ok(MessageDelivery::of(delivery))
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
    pub(crate) async fn payee_record(
        &self,
        record: &[u8; 32],
    ) -> Result<(ActivityTarget, ActivityRecord)> {
        let held = self
            .activity_write_base(record)
            .await?
            .ok_or(Refusal::NotFound)?;
        self.bound_payee_record(&held)
    }

    /// The accepted signed contents, bound to this key, for display and for identifying
    /// the immutable recipient of a request even when its mutable activity is disputed.
    pub(crate) fn bound_payee_record(
        &self,
        held: &pubky_switch_client::app::SharedRecord,
    ) -> Result<(ActivityTarget, ActivityRecord)> {
        let label = Collection::PayeeActivity
            .policy_label(&held.policy)
            .ok_or(Refusal::NotFound)?;
        let bound = self
            .bind_held(held, ActivitySide::Payee, label.as_ref())
            .ok_or(Refusal::NotFound)?;
        let target = ActivityTarget::payee(
            bound.natural_key_with(&self.identity(), &|endpoint, proof| {
                self.rail.settlement_id(endpoint, proof)
            })?,
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
    /// A store failure: the list is never partial.
    pub async fn payee_activity(&self) -> Result<Vec<PayeeActivity>> {
        self.refuse_while_unlisted().await?;
        // Each record is read and bound once, under the label its policy gives it, and its
        // period records are found among the same bound records, as `periods_of` finds them.
        let collection = Collection::PayeeActivity;
        let mut bound = Vec::new();
        for held in self.app.records_of(collection.name()).await? {
            match self.bound_payee_record(&held) {
                Ok((_, record)) => bound.push((held, record)),
                // A record that doesn't bind, or isn't held under a Paykit policy, isn't acted
                // on (Paykit wire 5.3): it is left out. A failure to read it is not.
                Err(PaykitError::Refused(Refusal::NotFound) | PaykitError::Lib(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let mut periods = self.group_periods(bound.iter().filter_map(|(held, record)| {
            let label = collection.policy_label(&held.policy)?;
            Some((held.key, label, record.clone()))
        }))?;
        let shared: std::collections::HashMap<[u8; 32], &pubky_switch_client::app::SharedRecord> =
            bound.iter().map(|(held, _)| (held.key, held)).collect();
        let mut out = Vec::new();
        for (held, record) in &bound {
            // A period record is shown with its request's.
            if record.request.is_none()
                && record
                    .binding()
                    .is_ok_and(|b| matches!(b, RecordBinding::Period { .. }))
            {
                continue;
            }
            let mut view = payee_view(held.key, record, None);
            if let Some(children) = periods.remove(&held.key) {
                view.periods = crate::periods::period_views(children, record);
            }
            // Shown divergent when its record, or one of its period records, is paused or
            // its latest view divergent (Paykit spec 6).
            let mut divergent = false;
            for key in std::iter::once(held.key).chain(view.periods.iter().map(|p| p.record)) {
                let pause = match shared.get(&key) {
                    Some(shared) => Box::pin(self.pause_seen_held(shared)).await?,
                    None => Box::pin(self.pause_seen(&key)).await?,
                };
                divergent |= pause.paused || pause.divergent;
            }
            view.divergent = divergent;
            out.push(view);
        }
        Ok(out)
    }

    /// The payee's own check of the proof in `record` (Paykit spec 8.1 step 4): the verified
    /// payment time, or `None` when the payment method doesn't confirm it. A Billing Period's
    /// endpoint is the request's, except that with per-period endpoints a later period is
    /// paid only to the destination of the valid details its proof names, so one paid
    /// elsewhere is `None`. An identifier absent from the selected endpoints, excluded by
    /// the request, or naming an app other than that
    /// source's signed owner is refused before the method's check. Named per-period rates
    /// price the payment through the same conversion rule as the payer's. The method's
    /// verified payment time must precede a selected details destination's expiry and, for
    /// a rate-priced payment, lie within those details' inclusive signed rate interval.
    /// Without fixed endpoints, the method verifies the destination itself.
    ///
    /// **One payment, one occurrence**, as in v0.1: a payment whose settlement this app has
    /// credited to another request, Billing Period or payment without a request of the same
    /// payer, that record holding a proof of the same settlement with its receipt, is `None`
    /// too. Its proof stays in its record and gets no receipt. The settlement is the one the
    /// payment method names ([`crate::Rail::settlement_id`]): whether one transaction can
    /// pay two occurrences is the method's rule, not Paykit's. A proof without a receipt,
    /// such as late evidence, credits nothing.
    ///
    /// # Errors
    ///
    /// [`Refusal::CertificateRefused`] while the app's certificate fails Paykit's checks;
    /// [`Refusal::NotFound`] for a record without a proof; [`Refusal::NotNow`] for a Billing
    /// Period's record whose request record the app holds but can't open or bind yet;
    /// [`Refusal::MaybeCredited`] while
    /// whether another record credits the settlement can't be told;
    /// [`Refusal::PaymentTimeOutside`] for a payment the method verified at a time the
    /// occurrence doesn't allow: before the request's `created_at` less the method's declared
    /// tolerance, after its payment deadline, or outside the validity of the destination or
    /// rates it was paid by, unless the record holds its receipt already; the rail's errors.
    pub async fn verify_payment(&mut self, record: &[u8; 32]) -> Result<Option<u64>> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.verify_payment_inner(record)).await
    }

    async fn verify_payment_inner(&mut self, record: &[u8; 32]) -> Result<Option<u64>> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        // The check goes on the heap, as `receive`'s steps do: it nests deep.
        Ok(Box::pin(self.verified(record, false, false))
            .await?
            .map(|credit| credit.paid_at))
    }

    /// Credit uses the accepted service write for both the proof and its request.
    /// A newer refused write may have changed the proof, receipt or restriction.
    pub(crate) async fn credit_input(
        &self,
        key: &[u8; 32],
    ) -> Result<Option<pubky_switch_client::app::SharedRecord>> {
        let held = self.activity_write_base(key).await?;
        if held.as_ref().is_some_and(|r| r.flagged || r.not_taken) {
            return Err(
                Refusal::NotNow("the current payee activity was not accepted".into()).into(),
            );
        }
        Ok(held)
    }

    /// [`Paykit::verify_payment`], the "can't tell" hold skipped when the user
    /// `maybe_credited_confirmed`.
    async fn verified(
        &mut self,
        record: &[u8; 32],
        maybe_credited_confirmed: bool,
        unmarked_credit_confirmed: bool,
    ) -> Result<Option<Box<VerifiedCredit>>> {
        if self.payee_deletion_held(record).await? {
            return Err(Refusal::CreditHistoryLost.into());
        }
        let mut conditions = vec![self.app.record_condition(record).await?];
        let held = self.credit_input(record).await?.ok_or(Refusal::NotFound)?;
        let (target, bound) = self.bound_payee_record(&held)?;
        if bound.lost_to_delete.is_some() {
            return Err(Refusal::CreditHistoryLost.into());
        }
        let parent = self.payee_period_parent(&target, &bound);
        if let Some(parent) = &parent {
            let key = self.activity_key(parent)?;
            if self.payee_deletion_held(&key).await? {
                return Err(Refusal::CreditHistoryLost.into());
            }
            conditions.push(self.app.record_condition(&key).await?);
        }
        let proof_stored = bound.proof.as_ref().ok_or(Refusal::NotFound)?;
        let Body::PaymentProof(proof) = proof_stored.body() else {
            return Err(Refusal::NotFound.into());
        };
        // A Billing Period's proof is of its request, whose record is the request's own; the
        // request decides whether its own destination or the named period details'
        // destination serves this occurrence (Paykit spec 8.2).
        let request_record = self.payee_request_record(&target, &bound).await?;
        if request_record
            .as_ref()
            .is_some_and(|parent| parent.lost_to_delete.is_some())
        {
            return Err(Refusal::CreditHistoryLost.into());
        }
        let id = &proof.payment_endpoint_identifier;
        let binding = bound.binding()?;
        let payment = match request_record.as_ref() {
            Some(parent) => {
                let Some((stored_request, request)) =
                    crate::periods::request_for_payment(parent, &bound, &binding)
                else {
                    return Ok(None);
                };
                if request
                    .accepted_payment_endpoint_identifiers
                    .as_ref()
                    .is_some_and(|accepted| !accepted.contains(id))
                {
                    return Ok(None);
                }
                // Named period endpoints do not relax the request's required app
                // (Paykit wire 4.5); this restriction applies to every destination source.
                if matches!(&request.destination, RequestDestination::RequiredApp(app)
                    if *app != proof.payment_app_id)
                {
                    return Ok(None);
                }
                // Invalid details have no effect (Paykit wire 4.9, 5.3). The request
                // still supplies a destination where its period rules permit it.
                let named_details = proof_stored.message().refs.get(1).and_then(|hash| {
                    crate::periods::valid_details(&bound, request)
                        .into_iter()
                        .find_map(|d| match d.body() {
                            Body::PaymentPeriodDetails(details) if d.hash() == *hash => {
                                Some((d, details))
                            }
                            _ => None,
                        })
                });
                // With per-period endpoints, the request's own serve only its first Billing
                // Period: a later one is credited only for a payment to the destination of
                // the period details its proof names (Paykit spec 8.2).
                let first = request
                    .recurrence
                    .as_ref()
                    .map(paykit_lib::periods::Recurrence::first)
                    .transpose()?;
                let later_period = request.per_period_endpoints
                    && proof.billing_period.is_some_and(|period| {
                        first.is_none_or(|first| first.starts_at != period.starts_at)
                    });
                // Named rates remain usable independently of the destination. The details'
                // endpoints, owner and expiry apply together, only to those later periods.
                let from_details =
                    named_details
                        .filter(|_| later_period)
                        .and_then(|(stored, details)| {
                            details.payment_endpoints.as_ref().map(|endpoints| {
                                (stored.message().from.app_id, endpoints, details.expires_at)
                            })
                        });
                let destination_expires = from_details.map(|(_, _, expires)| expires);
                if later_period && from_details.is_none() {
                    return Ok(None);
                }
                // An endpoint map and its signed owner are one destination source (Paykit
                // wire 4.4). Select them together before looking up the identifier; neither
                // an absent identifier nor another app falls back to another source.
                let endpoints = match from_details {
                    Some((owner, endpoints, _)) => Some((owner, endpoints)),
                    None => match &request.destination {
                        RequestDestination::Endpoints(endpoints) => {
                            Some((stored_request.message().from.app_id, endpoints))
                        }
                        RequestDestination::RequiredApp(_) | RequestDestination::Any => None,
                    },
                };
                let payload = match endpoints {
                    Some((owner, endpoints)) => {
                        if owner != proof.payment_app_id {
                            return Ok(None);
                        }
                        match endpoints.get(id) {
                            Some(payload) => Some(payload.clone()),
                            None => return Ok(None),
                        }
                    }
                    None => None,
                };
                // The amount the payer computed for the endpoint its proof names, by the same
                // rule (Paykit spec 7.2 step 1), with this app's payment method's unit: the
                // request's amount, rounded up once to the unit of an endpoint of its asset;
                // under fixed or named per-period rates, only the computed amount is
                // credited. An endpoint the terms leave unusable is not credited.
                let request_ref = RequestRef {
                    payee: self.identity(),
                    request_hash: stored_request.hash(),
                };
                let mut pricing = crate::pay::Pricing::of_request(
                    &request.amount,
                    request_ref,
                    proof.billing_period,
                    request.conversion.as_ref(),
                    pubky_switch_client::app::Initiator::User,
                );
                if let (Some(Conversion::PerPeriod), Some((stored, _))) =
                    (request.conversion.as_ref(), named_details)
                {
                    pricing = pricing.with_period_rates(vec![stored]);
                }
                // The payee checks the method's verified payment time below, even when
                // the proof arrives after the period rate expired.
                let (price, source) = pricing.price(&*self.rail, id, |_| true);
                let (amount, rate_window) = match price {
                    crate::conversion::Priced::Plain(amount)
                    | crate::conversion::Priced::ByHand(amount) => (amount, None),
                    crate::conversion::Priced::ByRate { amount, .. } => {
                        (amount, source.and_then(crate::periods::rate_window))
                    }
                    crate::conversion::Priced::Unusable(_) => return Ok(None),
                };
                // The last payment time the occurrence allows, inclusive: a Billing Period's
                // deadline, or a one-time request's payment deadline (Paykit spec 7.1). The
                // proposal expiry bounds the attempt only (Paykit wire 4.5), so a payment
                // started inside it and settled just after is still credited, as in v0.1.
                let deadline = match binding {
                    RecordBinding::Period { billing_period, .. } => {
                        request.period_payment_deadline(&billing_period)?
                    }
                    RecordBinding::Request { .. } => request.payment_deadline_ms(),
                    RecordBinding::Unrequested => None,
                };
                // Evidence dated before the request doesn't prove it, less the tolerance the
                // payment method declares for evidence dated early (Paykit spec 7.1, "Not
                // before the request"): the request's signed `created_at`, which every own app
                // reads alike, is the time the payee recorded it.
                let not_before = stored_request
                    .message()
                    .created_at_ms
                    .saturating_sub(self.rail.declaration().payment_time_tolerance_ms);
                (
                    amount,
                    payload,
                    destination_expires,
                    rate_window,
                    deadline,
                    Some(not_before),
                )
            }
            // A period record whose request record the app holds but can't open or bind yet,
            // sealed under a scope key it doesn't hold or a value that doesn't read: not now,
            // as `payer_request` answers for a request record held but not open; the credit
            // is tried again once it opens.
            None if self.request_record_held(&target, &bound).await? => {
                return Err(Refusal::NotNow(
                    "Paykit wire 5.3: the period's request record is held but not open".into(),
                )
                .into());
            }
            None if binding == RecordBinding::Unrequested => (
                proof.amount.clone().ok_or(Refusal::NotFound)?,
                None,
                None,
                None,
                None,
                None,
            ),
            None => return Err(Refusal::NotFound.into()),
        };
        let (amount, payload, destination_expires, rate_window, deadline, not_before) = payment;
        let settlement_id = self
            .rail
            .settlement_id(&proof.payment_endpoint_identifier, &proof.proof);
        let mut other_uncertain = false;
        if let Some(settlement) = settlement_id.as_ref().filter(|_| bound.receipt.is_none()) {
            if self
                .retained_credit(bound.receipt.as_ref().map(|r| r.hash()), settlement)
                .await?
            {
                return Ok(None);
            }
            match self
                .other_credit(record, &bound.counterparty, settlement)
                .await?
            {
                OtherCredit::Credited => return Ok(None),
                OtherCredit::CantTell => other_uncertain = true,
                _ => {}
            }
        }
        let check = ProofCheck {
            endpoint_id: proof.payment_endpoint_identifier.clone(),
            payload,
            amount,
            attempt_id: proof.attempt_id,
            proof: proof.proof.clone(),
        };
        // The method's evidence dates the payment, never this proof's arrival. It must be
        // no earlier than the request, within the method's tolerance, and no later than the
        // occurrence's deadline. A details destination must be unexpired; rates include both
        // signed interval boundaries (Paykit spec 7.1, 7.2, 8.2, Paykit wire 4.9). Same-asset
        // payments use no rate interval. The tolerance widens only the lower bound.
        let Some(paid_at) = self.rail.verify(check).await? else {
            return Ok(None);
        };
        let in_time = not_before.is_none_or(|earliest| paid_at >= earliest)
            && deadline.is_none_or(|deadline| paid_at <= deadline)
            && destination_expires.is_none_or(|expires| paid_at < expires)
            && rate_window.is_none_or(|(start, expires)| {
                paykit_lib::periods::rates_apply(start, expires, paid_at)
            });
        // The method verified the payment, but not within what the occurrence allows: the
        // evidence is flagged, never credited on its own (Paykit spec 7.1, "Not before the
        // request"). The funds are the payee's received funds all the same.
        // A record that holds the receipt already is credited: another own app's method may
        // date the evidence differently, and the credit stands (Paykit spec 8.1 step 4).
        if bound.receipt.is_none() && !in_time {
            return Err(Refusal::PaymentTimeOutside {
                paid_at_ms: paid_at,
            }
            .into());
        }
        let settlement = if bound.receipt.is_some() {
            None
        } else {
            if settlement_id.is_none() && binding == RecordBinding::Unrequested {
                return Ok(None);
            }
            let loss = self.settlement_loss().await?;
            let uncertain = loss.is_some_and(|loss| loss.settlement_uncertain(paid_at));
            let decision = if let Some(id) = &settlement_id {
                match Box::pin(self.settlement_credit(
                    id,
                    &target,
                    bound.counterparty,
                    binding,
                    crate::settlements::CreditConsent {
                        loss,
                        uncertain,
                        maybe_credited: maybe_credited_confirmed,
                        unmarked: unmarked_credit_confirmed,
                    },
                ))
                .await?
                {
                    SettlementCredit::SameOccurrence => {
                        // A competing app stored the receipt in the same batch as this
                        // mark. Read and verify the signed activity before returning it.
                        let read = self.app.read_record_fresh(&target.record_target()).await?;
                        let RecordRead::Fresh(current) = read else {
                            return Ok(None);
                        };
                        if self.bound_payee_record(&current)?.1.receipt.is_some() {
                            return Box::pin(self.verified(
                                record,
                                maybe_credited_confirmed,
                                unmarked_credit_confirmed,
                            ))
                            .await;
                        }
                        return Ok(None);
                    }
                    SettlementCredit::AlreadyCredited => return Ok(None),
                    SettlementCredit::Write(write) => Some(*write),
                }
            } else {
                None
            };
            if other_uncertain || uncertain || settlement_id.is_none() {
                let subject = match &settlement_id {
                    Some(id) => crate::SettlementSubject::Tag(paykit_lib::derive::settlement_tag(
                        self.app.naming_key(),
                        id,
                    )),
                    None => crate::SettlementSubject::RequestedProof {
                        record: *record,
                        proof: proof_stored.hash(),
                    },
                };
                self.raise_settlement_notice(subject, bound.counterparty)
                    .await?;
                if !maybe_credited_confirmed {
                    return Err(Refusal::MaybeCredited.into());
                }
            }
            decision
        };
        Ok(Some(Box::new(VerifiedCredit {
            paid_at,
            target,
            activity: bound,
            parent,
            conditions,
            settlement,
        })))
    }

    /// Whether another `payee_activity` record than `record` credits `settlement` from
    /// any payer: its signed proof and receipt belong to the same occurrence (Paykit spec
    /// 6 and 8.1 step 4). When the app cannot open a held value or its copy may lack
    /// records, it cannot tell whether another occurrence was credited. A record under a
    /// policy that isn't Paykit's is ignored.
    async fn other_credit(
        &self,
        record: &[u8; 32],
        payer: &[u8; 32],
        settlement: &[u8],
    ) -> Result<OtherCredit> {
        // Both callers list the records first ([`Paykit::records_known`]), which refuses
        // while the copy may lack records; held here all the same, a copy that may lack
        // records can't tell.
        let mut cant_tell = self.side_unlisted(crate::hold::RecordSide::Payee).await?;
        for held in self
            .app
            .records_of(Collection::PayeeActivity.name())
            .await?
        {
            // A record under a policy Paykit doesn't use is ignored, as every reader ignores
            // it (Paykit spec 6, Paykit wire 5.1): it holds no credit of Paykit's.
            if held.key == *record
                || Collection::PayeeActivity
                    .policy_label(&held.policy)
                    .is_none()
            {
                continue;
            }
            // A value of a Paykit policy that doesn't read or bind can't be told; a failure
            // to read it is the call's error, never "no other credit".
            let other = match held.content {
                RecordContent::Deleted if !held.different_write_at_held_version => continue,
                RecordContent::Value(_) => match self.bound_payee_record(&held) {
                    Ok(bound) => Some(bound),
                    Err(PaykitError::Refused(Refusal::NotFound) | PaykitError::Lib(_)) => None,
                    Err(error) => return Err(error),
                },
                _ => None,
            };
            if held.different_write_at_held_version {
                if let Some((target, other)) = &other {
                    if self
                        .disputed_credit_unrelated(target, other, payer, settlement)
                        .await?
                    {
                        continue;
                    }
                }
                return Err(crate::records::activity_base_refusal());
            }
            let Some((target, other)) = other else {
                cant_tell = true;
                continue;
            };
            if self.credits_settlement(&other, settlement) {
                let parent = self.payee_request_record(&target, &other).await?;
                match receipt_binding(&other, parent.as_ref()) {
                    ReceiptBinding::Verified => return Ok(OtherCredit::Credited),
                    ReceiptBinding::MissingRequest => cant_tell = true,
                    ReceiptBinding::Invalid => {}
                }
            }
        }
        Ok(if cant_tell {
            OtherCredit::CantTell
        } else {
            OtherCredit::None
        })
    }

    /// Exclude a disputed activity only from evidence fixed by its key. The signed
    /// request fixes the payer even when another write at its version is disputed.
    async fn disputed_credit_unrelated(
        &self,
        target: &ActivityTarget,
        other: &ActivityRecord,
        payer: &[u8; 32],
        settlement: &[u8],
    ) -> Result<bool> {
        // The accepted copy still holds this settlement's receipt. Another payer
        // on the requested payment does not resolve the disputed write.
        if self.credits_settlement(other, settlement) {
            return Ok(false);
        }
        if let Some(request) = &other.request {
            return Ok(request.message().to != *payer);
        }
        if let Some(target) = self.payee_period_parent(target, other) {
            let request = self.held_activity(&target).await?;
            return Ok(request
                .as_ref()
                .and_then(request_of)
                .is_some_and(|(signed, _)| signed.message().to != *payer));
        }
        // An unrequested payee record is keyed by endpoint and settlement. The
        // binding already required a readable proof with a settlement for this key.
        Ok(other
            .proof
            .as_ref()
            .is_some_and(|stored| match stored.body() {
                Body::PaymentProof(proof) => self
                    .rail
                    .settlement_id(&proof.payment_endpoint_identifier, &proof.proof)
                    .is_some_and(|id| id != settlement),
                _ => false,
            }))
    }

    /// Verify the proof in `record` and, when the payment method confirms it, send the
    /// payer a receipt (Paykit spec 8.1 step 4, Paykit wire 4.11), recorded in the record in
    /// the batch that queues it: a credit always leaves its receipt in the record. Returns
    /// the receipt's `msg_id`, or `None` when the method doesn't confirm the payment, or
    /// its settlement is credited to another occurrence already ([`Paykit::verify_payment`]).
    ///
    /// # Errors
    ///
    /// As [`Paykit::verify_payment`]; the client's errors; [`crate::Refusal::Divergent`] while the
    /// record, or another of the same payer, was seen divergent and its user hasn't
    /// confirmed it as the refusal shows it, that it may hold a credit
    /// ([`crate::DivergentRecord::may_hold_credit`]), or the messages kept over it wait to
    /// be written.
    pub async fn confirm_payment(&mut self, record: &[u8; 32]) -> Result<Option<String>> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
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
    /// receipt of a request, whose reference applies, with nothing sent. With
    /// [`ReceiptDetails::with_maybe_credited_confirmed`], [`Refusal::MaybeCredited`] isn't
    /// given: the payment is credited unless a credit the app can see refuses it.
    pub async fn confirm_payment_with(
        &mut self,
        record: &[u8; 32],
        details: ReceiptDetails,
    ) -> Result<Option<String>> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.confirm_payment_with_inner(record, details)).await
    }

    async fn confirm_payment_with_inner(
        &mut self,
        record: &[u8; 32],
        details: ReceiptDetails,
    ) -> Result<Option<String>> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        let confirmed = details.maybe_credited_confirmed.is_some();
        let mut tries = 0;
        let msg_id = loop {
            // A failed conditional write can reveal a deletion or a changed proof.
            let Some(credit) = Box::pin(self.verified(
                record,
                confirmed,
                details.unmarked_credit_confirmed.is_some(),
            ))
            .await?
            else {
                return Ok(None);
            };
            let VerifiedCredit {
                target,
                mut activity,
                parent,
                conditions,
                settlement,
                ..
            } = *credit;
            let bound = &activity;
            if let Some(receipt) = &bound.receipt {
                return Ok(Some(receipt.message().msg_id.clone()));
            }
            // A credit waits for the user while this record, or another of the same
            // counterparty, is divergent or was seen so: its upload may hold a receipt of the
            // same settlement (Paykit spec 6, 8.1 step 4).
            Box::pin(self.refuse_credit_while_paused(record, &bound.counterparty)).await?;
            let proof_stored = bound.proof.as_ref().ok_or(Refusal::NotFound)?;
            let Body::PaymentProof(proof) = proof_stored.body() else {
                return Err(Refusal::NotFound.into());
            };
            let mut refs = vec![proof_stored.hash()];
            match bound.binding()? {
                RecordBinding::Request { request_hash }
                | RecordBinding::Period { request_hash, .. } => refs.push(request_hash),
                RecordBinding::Unrequested => {}
            }
            // The receipt carries the proof's attempt and Billing Period (Paykit wire 4.11).
            let body = PaymentReceipt {
                attempt_id: proof.attempt_id,
                billing_period: proof.billing_period,
                metadata: details.metadata.clone(),
                payment_reference: details.payment_reference.clone(),
            }
            .to_body(refs.len())?;
            // Write the receipt into the same verified activity and queue it in that
            // batch. A changed proof or parent requires a fresh payment-method decision.
            let signed = self.sign_for_sale(
                &target,
                bound.counterparty,
                Kind::PaymentReceipt,
                refs,
                body,
            )?;
            let msg_id = signed.msg_id().to_owned();
            if !activity::place(&mut activity, Role::Receipt, self.own_copy(&signed)?) {
                return Err(
                    Refusal::NotNow("the receipt message ID was already used".into()).into(),
                );
            }
            let target = target.record_target();
            let entry = self
                .app
                .record_op(&target, Some(&activity.to_json()?), conditions)
                .await?;
            let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME).entry(entry);
            let delivery_marker = (!details.store_only)
                .then(|| key::receipt_batch(record, &signed.plaintext().hash(), &msg_id, false));
            if let Some(marker) = &delivery_marker {
                batch = batch.keep_id_at(marker.as_bytes().to_vec()).local(
                    pubky_switch_client::app::LocalWrite::Delete {
                        key: marker.as_bytes().to_vec(),
                    },
                );
                batch = batch.queue(signed);
            }
            let submitted = match settlement {
                Some(write) => {
                    Box::pin(self.submit_settlement_credit(
                        batch,
                        write,
                        details.unmarked_credit_confirmed.is_some(),
                        delivery_marker.as_deref(),
                    ))
                    .await
                }
                None => self.submit(batch).await.map_err(PaykitError::from),
            };
            if let (Err(error), Some(marker)) = (&submitted, &delivery_marker) {
                if let Some(batch) = error.kept_batch() {
                    // Preserve the original submission's ID even if saving its evidence fails.
                    let _ =
                        Box::pin(self.remember_receipt_application(&target, marker, batch)).await;
                } else {
                    self.retire_refused_receipt_batch(Some(marker)).await?;
                }
            }
            match submitted {
                Ok(_) => break msg_id,
                Err(PaykitError::Client(error)) if error.code() == Some(ErrorCode::Conflict) => {
                    self.app.read_record(&target).await?;
                    if let Some(parent) = &parent {
                        self.app.read_record(&parent.record_target()).await?;
                    }
                    tries += 1;
                    if tries >= WRITE_TRIES {
                        return Err(conflict(error.into()));
                    }
                }
                Err(error) => return Err(error),
            }
        };
        if !details.store_only {
            self.send_message(&msg_id).await?;
        }
        Ok(Some(msg_id))
    }

    /// The payee cancels a request it sent (Paykit spec 8.1, Paykit wire 4.8). The
    /// cancellation carries no label and goes only to the payer's apps holding `paykit_pay`
    /// (Paykit spec 3.4). A proof that crosses it still counts as payment.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`]; [`Refusal::Cancelled`] if it was already; the client's errors.
    pub async fn cancel_sent_request(
        &mut self,
        record: &[u8; 32],
        reason: Option<String>,
    ) -> Result<String> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.cancel_sent_request_inner(record, reason)).await
    }

    async fn cancel_sent_request_inner(
        &mut self,
        record: &[u8; 32],
        reason: Option<String>,
    ) -> Result<String> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
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
            let signed = self.sign_for_sale(
                &target,
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
                    label: target.label().map(|label| label.as_str().to_owned()),
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
        self.send_message(&msg_id).await?;
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
    /// The refusal of a call on the payer's request record at `target`, which the app doesn't
    /// hold open: not now for a record held that it can't open, sealed under a scope key it
    /// doesn't hold yet or a value that doesn't read, and for one absent while the copy of
    /// the payer's records may lack records (Paykit spec 8.1, "Records not listed yet"):
    /// either may hold a response or a cancellation. [`Refusal::NotFound`] only for a record
    /// absent or deleted from a copy that lacks none.
    pub(crate) async fn request_record_refusal(
        &self,
        target: &ActivityTarget,
    ) -> Result<PaykitError> {
        let content = self
            .app
            .shared_record(&self.activity_key(target)?)
            .await?
            .map(|held| held.content);
        Ok(match content {
            Some(content) if crate::hold::awaits_key(&content) => crate::hold::value_awaits_key(),
            Some(RecordContent::Value(_) | RecordContent::Unreadable(_)) => {
                Refusal::NotNow("Paykit wire 5.3: the request record held doesn't read".into())
                    .into()
            }
            _ if self.side_unlisted(crate::hold::RecordSide::Payer).await? => {
                crate::hold::unlisted()
            }
            _ => Refusal::NotFound.into(),
        })
    }

    pub(crate) async fn read_payer_request(
        &mut self,
        request: &RequestRef,
    ) -> Result<(ActivityTarget, ActivityRecord)> {
        let target = ActivityTarget::payer(natural_key::request_record(
            &request.payee,
            &request.request_hash,
        ));
        let Some(record) = self.activity(&target).await? else {
            return Err(self.request_record_refusal(&target).await?);
        };
        if request_of(&record).is_none() {
            return Err(Refusal::NotFound.into());
        }
        Ok((target, record))
    }

    pub(crate) async fn payer_request(
        &mut self,
        request: &RequestRef,
    ) -> Result<(ActivityTarget, ActivityRecord)> {
        let (target, record) = self.read_payer_request(request).await?;
        // The kept stop refuses whatever this placement does; it is tried again later.
        if self.place_kept_cancellation(&target).await.unwrap_or(false) {
            let record = self.activity(&target).await?.ok_or(Refusal::NotFound)?;
            return Ok((target, record));
        }
        Ok((target, record))
    }

    /// Place the cancellation kept for the request record at `target`, if one stops it, and
    /// retire its kept copies: once placed, the record holds a cancellation, one per role
    /// (Paykit spec 6). Further reads retire any keys past the batch's local limit. Copies
    /// none of which stops the request, those its label doesn't let
    /// cancel it once it reads ([`Paykit::kept_stop_message`]), are dropped and nothing is
    /// placed. Returns whether a cancellation was placed.
    async fn place_kept_cancellation(&mut self, target: &ActivityTarget) -> Result<bool> {
        let copies = self.kept_stop_copies(target).await?;
        if copies.is_empty() {
            return Ok(false);
        }
        let record = self.held_activity(target).await?;
        if record.as_ref().and_then(request_of).is_none() {
            return Ok(false);
        }
        let mut tx = Tx::default();
        for key in crate::receive::cancellation_keys_to_retire(copies.keys(), 0) {
            tx.delete(key);
        }
        if record
            .as_ref()
            .is_some_and(|record| record.cancellation.is_some())
        {
            self.local_commit(tx).await?;
            return Ok(false);
        }
        let Some(cancellation) = first_kept_stop(copies.into_values(), record.as_ref(), |a, b| {
            self.app.received_order(a, b)
        })?
        else {
            self.local_commit(tx).await?;
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
        match write {
            Some(entry) => {
                let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
                    .entry(entry)
                    .locals(tx.into_writes());
                if let Err(error) = self.submit(batch).await {
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

    /// Whether this app keeps a payee's cancellation that stops the request record at
    /// `target`, the kept-copy stop (Paykit spec 8.1): read by every path that makes a new
    /// attempt on the request, and by its view, so a stop not yet placed in the record still
    /// refuses it.
    pub(crate) async fn kept_stop(&self, target: &ActivityTarget) -> Result<bool> {
        Ok(self.kept_stop_message(target).await?.is_some())
    }

    /// A payee's cancellation kept as a stop of the request record at `target` that stops it,
    /// the earliest accepted: of the kept copies, from their original certificates and feed generations, those
    /// [`stop_applies`] takes against the record as the app holds it now. The one place a
    /// kept stop is judged outside the receive step.
    pub(crate) async fn kept_stop_message(
        &self,
        target: &ActivityTarget,
    ) -> Result<Option<StoredMessage>> {
        // No kept copy stops nothing, whatever the record holds: it is read only for copies.
        let copies = self.kept_stop_copies(target).await?;
        if copies.is_empty() {
            return Ok(None);
        }
        let record = self.held_activity(target).await?;
        first_kept_stop(copies.into_values(), record.as_ref(), |a, b| {
            self.app.received_order(a, b)
        })
    }

    /// Every cancellation kept as a stop of the request record at `target`, by its key, each
    /// read bound to its key, as a restore reads it (`state::Stored`): one that fails it is a
    /// corrupt store, never no stop.
    async fn kept_stop_copies(
        &self,
        target: &ActivityTarget,
    ) -> Result<std::collections::BTreeMap<String, (ReceivedAt, StoredMessage)>> {
        let prefix = key::kept_cancellations(&self.activity_key(target)?);
        let own = self.identity();
        let mut copies = std::collections::BTreeMap::new();
        for (rest, kept) in self.local_scan::<KeptMessage>(&prefix).await? {
            copies.insert(
                format!("{prefix}{rest}"),
                (kept.received_at()?, kept.stored(&own)?),
            );
        }
        Ok(copies)
    }

    /// Place every kept cancellation whose request record this app holds, each as a later
    /// write of its own with the kept copy dropped (Paykit spec 8.1). Run after each
    /// receive, at a resume, and before any payment. One that can't be placed now stays
    /// kept, for the next run; its kept copy refuses every new attempt meanwhile. A stop
    /// for a request the copy doesn't hold ([`crate::receive::holds_record`]) goes once
    /// [`KEPT_MESSAGE_MS`] have passed since the cancellation's acceptance, on the highest
    /// service time verified, but never while the copy of the payer's records may lack
    /// records, when the absence decides nothing, nor while the app keeps a stored message
    /// of that request to write ([`crate::stored::BoundNote::writes_request`]): the request's own entry
    /// may be one, waiting for a write that failed or for a listing, and written without
    /// the stop the request would be payable.
    pub(crate) async fn place_kept_cancellations(&mut self) -> Result<()> {
        let mut placed = std::collections::BTreeSet::new();
        let own = self.identity();
        for (rest, kept) in self
            .local_scan::<KeptMessage>(key::KEPT_CANCELLATIONS)
            .await?
        {
            // Read bound to its key, as a restore reads it: a stop that fails it is a corrupt
            // store, so the receive fails rather than drop or place it.
            let cancellation = kept.stored(&own)?;
            let stop_key = format!("{}{rest}", key::KEPT_CANCELLATIONS);
            let Some(request) = cancellation.message().refs.first().copied() else {
                return Err(PaykitError::corrupt());
            };
            if placed.contains(&(cancellation.sender(), request)) {
                continue;
            }
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
                placed.insert((cancellation.sender(), request));
                if let Err(error) = self.place_kept_cancellation(&target).await {
                    crate::pay::unless_stop(error)?;
                }
                continue;
            }
            let held = self.app.shared_record(&self.activity_key(&target)?).await?;
            // A kept stop always has its acceptance time (the check refuses one without).
            let stale = cancellation.accepted_at().is_some_and(|at| {
                self.service_time()
                    .is_some_and(|t| at.saturating_add(KEPT_MESSAGE_MS) <= t)
            });
            if stale
                && !crate::receive::holds_activity(held.as_ref())
                && !self.side_unlisted(crate::hold::RecordSide::Payer).await?
                && !self
                    .keeps_stored_of(&cancellation.sender(), &request)
                    .await?
            {
                self.local_delete(&stop_key).await?;
            }
        }
        Ok(())
    }

    /// Whether the app keeps a stored message from `sender` of `request` still to write
    /// ([`crate::stored::BoundNote::writes_request`]).
    async fn keeps_stored_of(&self, sender: &[u8; 32], request: &[u8; 32]) -> Result<bool> {
        for (_, note) in self
            .local_scan::<crate::stored::BoundNote>(&key::stored_notes_of(sender))
            .await?
        {
            if note.writes_request(request) {
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
        // Each record is read and bound once, and its period records are found among the
        // same bound records: under the fixed policy, as `periods_of` finds them.
        let collection = Collection::PayerActivity;
        let mut bound = Vec::new();
        for held in self.app.records_of(collection.name()).await? {
            if let Some(record) = self.bind_held(&held, ActivitySide::Payer, None) {
                bound.push((held, record));
            }
        }
        let mut periods = self.group_periods(
            bound
                .iter()
                .filter(|(held, _)| collection.policy_label(&held.policy) == Some(None))
                .map(|(held, record)| (held.key, None, record.clone())),
        )?;
        let mut reads = ListReads::default();
        let mut out = Vec::new();
        for (held, record) in &bound {
            let held_periods = periods.remove(&held.key).unwrap_or_default();
            if let Some(view) = self
                .request_view(held.key, record, held_periods, Some(held), &mut reads)
                .await?
            {
                out.push(view);
            }
        }
        let mut shown: std::collections::HashSet<([u8; 32], [u8; 32])> = out
            .iter()
            .map(|view| (view.request.payee, view.request.request_hash))
            .collect();
        for view in self.unrecorded_requests().await? {
            if shown.insert((view.request.payee, view.request.request_hash)) {
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
        let key = self.activity_key(&target)?;
        let held = self.app.shared_record(&key).await?;
        let Some(record) = self.read_activity(&target, held.as_ref()) else {
            return self
                .unrecorded_requests()
                .await?
                .into_iter()
                .find(|view| view.request == *request)
                .ok_or_else(|| Refusal::NotFound.into());
        };
        let periods = self
            .periods_of(Collection::PayerActivity)
            .await?
            .remove(&key)
            .unwrap_or_default();
        self.request_view(
            key,
            &record,
            periods,
            held.as_ref(),
            &mut ListReads::default(),
        )
        .await?
        .ok_or_else(|| Refusal::NotFound.into())
    }

    /// The view of the request in `record`, held at `record_key` as `held`, with the values
    /// a list read shares across its records taken from `reads`.
    async fn request_view(
        &self,
        record_key: [u8; 32],
        record: &ActivityRecord,
        periods: Vec<([u8; 32], ActivityRecord)>,
        held: Option<&pubky_switch_client::app::SharedRecord>,
        reads: &mut ListReads,
    ) -> Result<Option<RequestView>> {
        let Some((stored, request)) = request_of(record) else {
            return Ok(None);
        };
        let periods = crate::periods::period_views(periods, record);
        let payee = record.counterparty;
        let hash = stored.hash();
        let mut status = self.request_status(record, request, &payee, &hash).await?;
        // A cancellation kept for the request and not placed yet is a cancellation all the
        // same (Paykit spec 8.1), a subscription's included (Paykit spec 8.2), read with a
        // restore's check. A subscription that has ended is a view of the time (Paykit spec
        // 8.2), so a stop, the restore's hold and the deny list override it as they do an
        // open request's status.
        let live = |status: &RequestStatus| {
            matches!(
                status,
                RequestStatus::Open | RequestStatus::Subscribed | RequestStatus::Ended
            )
        };
        let target = ActivityTarget::payer(natural_key::request_record(&payee, &hash));
        // Once the bound record contains the cancellation, its view no longer needs to
        // order any remaining local copies from before the record was readable.
        let stopped = !cancelled(record) && self.kept_stop(&target).await?;
        if live(&status) && stopped {
            status = RequestStatus::Cancelled;
        }
        // Whether the request is still open, taken before the status is overlaid for display
        // with a restore's hold, a block or a divergent record: none of them ends the request,
        // and the block floor holds an open one whatever its status shows.
        let open = live(&status);
        let shared = held;
        let held = if record.lost_to_delete.is_none()
            && self.held_with(&mut reads.hold, &record_key, record).await?
        {
            if live(&status) {
                status = RequestStatus::Held;
            }
            Some(self.held_request_with(&mut reads.hold, &record_key).await?)
        } else {
            None
        };
        if live(&status) && self.denied_with(&mut reads.denied, &payee).await? {
            status = RequestStatus::Blocked;
        }
        // The record's mark, or, for a request still open, the block floor (`crate::floor`):
        // no automatic attempt is made on it either way.
        let manual_only =
            record.manual_only || (open && self.floor_holds_with(&mut reads.floor, stored).await?);
        // Divergent shows on its own field whatever the status, so a request held after a
        // restore or blocked shows that too; the status says so only when nothing else holds
        // the request.
        let pause = match shared {
            Some(shared) => Box::pin(self.pause_seen_held(shared)).await?,
            None => Box::pin(self.pause_seen(&record_key)).await?,
        };
        let divergent = pause.paused || pause.divergent;
        if status == RequestStatus::Open && pause.paused {
            status = RequestStatus::Divergent;
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
            recurrence: request.recurrence,
            status,
            held,
            divergent,
            receipt: received_receipt(record_key, record, Some(request)),
            manual_only,
            periods,
        }))
    }

    /// Whether the highest service time verified is at or past `deadline`.
    fn reached(&self, deadline: Option<u64>) -> bool {
        deadline.is_some_and(|deadline| self.service_time().is_some_and(|t| deadline <= t))
    }

    /// Whether the request in `record` may still be answered as `answer` says (Paykit spec
    /// 8.1, 8.2): not paid, rejected or cancelled, nor `stopped` by a cancellation the app
    /// keeps. A recurring request is accepted or rejected only before it is accepted, and
    /// cancelled before or after; a one-time request is accepted only by paying it. Its
    /// expiry is judged apart, on the record read again ([`Paykit::expired_answer`]).
    fn refuse_answer(
        &self,
        record: &ActivityRecord,
        body: &PaymentRequest,
        answer: Answer,
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
        match (body.is_recurring(), answer) {
            (true, Answer::Cancel) => Ok(()),
            (true, _) if record.response.is_some() => Err(Refusal::Subscribed.into()),
            (false, Answer::Accept) => Err(Refusal::Usage(
                "Paykit spec 8.1: a one-time request is accepted by paying it".into(),
            )
            .into()),
            // One response per request (Paykit spec 8.1, "Rejecting and cancelling"): after
            // an acceptance the payer may cancel, after a rejection it neither accepts nor
            // cancels, and after a cancellation it sends no response. A one-time request
            // accepted, by this app or another own app whose acceptance the record holds, is
            // cancelled, not rejected, the acceptance's payment having failed included: the
            // claim then shows no open, `unknown` or paid attempt, so the cancellation goes.
            (false, Answer::Reject) if accepted(record) => {
                Err(Refusal::Usage("accepted; cancel instead".into()).into())
            }
            _ => Ok(()),
        }
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
        Ok(self.fresh_request_stamped(target).await?.0)
    }

    /// [`Paykit::fresh_request`], with the stamp of the write the read shows current
    /// ([`WriteStamp`]) when it is [`Fresh::Held`], for a check that reads the record again
    /// and fails closed when the write changed ([`Paykit::automatic_dispatch_held`]).
    ///
    /// # Errors
    ///
    /// As [`Paykit::fresh_request`].
    pub(crate) async fn fresh_request_stamped(
        &mut self,
        target: &ActivityTarget,
    ) -> Result<(Fresh, Option<WriteStamp>)> {
        let record = match self.app.read_record_fresh(&target.record_target()).await? {
            RecordRead::Fresh(record) => record,
            RecordRead::Kept { reason, .. } => return Ok((fresh_kept(reason)?, None)),
            // A read a later client names: nothing shows the switch's write held now.
            _ => return Ok((Fresh::Unreadable, None)),
        };
        let stamp = WriteStamp::of(&record);
        Ok(match record.content {
            RecordContent::Deleted => (Fresh::Missing, None),
            RecordContent::Value(_) => match self.held_activity(target).await? {
                Some(record) => (Fresh::Held(Box::new(record)), Some(stamp)),
                None => (Fresh::Unreadable, None),
            },
            _ => (Fresh::Unreadable, None),
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
        answer: Answer,
        otherwise: Option<PaykitError>,
    ) -> Result<PaykitError> {
        Ok(match self.fresh_request(target).await? {
            Fresh::Held(fresh) => {
                let Some((_, body)) = request_of(&fresh) else {
                    return Ok(otherwise.unwrap_or_else(unreadable_request));
                };
                if let Err(closed) = self.refuse_answer(&fresh, body, answer, false) {
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
        if request.is_recurring() {
            return Ok(self.recurring_status(record, request));
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
        if record.lost_to_delete.is_some() {
            return Ok(RequestStatus::MayBePaid);
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
        // A loss can leave no claim at the new service while another app paid before it
        // (Paykit spec 7.3).
        if self
            .in_doubt_after_loss(
                &obligation,
                request_of(record).map(|(stored, _)| stored),
                None,
            )
            .await?
        {
            return Ok(RequestStatus::MayBePaid);
        }
        // Expired once the deadline is at or before the highest service time verified (Paykit
        // spec 7.1): a one-time request's attempt deadline.
        if self.reached(expiry_deadline(record, request)) {
            return Ok(RequestStatus::Expired);
        }
        Ok(RequestStatus::Open)
    }

    /// What the payer sees of a recurring request, from its request record: rejected or
    /// cancelled; accepted, [`RequestStatus::Subscribed`] until its last Billing Period has
    /// ended on the highest service time verified, [`RequestStatus::Ended`] then; or, not
    /// accepted, expired past its proposal expiry (v0.1's `proposal_expired`), else open.
    fn recurring_status(&self, record: &ActivityRecord, request: &PaymentRequest) -> RequestStatus {
        if rejected(record) {
            return RequestStatus::Rejected;
        }
        if cancelled(record) {
            return RequestStatus::Cancelled;
        }
        if record.response.is_some() {
            let last_end = request
                .recurrence
                .as_ref()
                .and_then(|r| r.last().ok().flatten())
                .map(|last| last.ends_at);
            return if self.reached(last_end) {
                RequestStatus::Ended
            } else {
                RequestStatus::Subscribed
            };
        }
        if self.reached(expiry_deadline(record, request)) {
            return RequestStatus::Expired;
        }
        RequestStatus::Open
    }

    /// Reject a request (Paykit spec 8.1, Paykit wire 4.7): recorded first, conditioned on
    /// its claim having no open or paid attempt, then sent. Returns the rejection's `msg_id`.
    /// A recurring request is rejected before its acceptance, conditioned on its proposal
    /// not having expired, as its acceptance is ([`Paykit::accept_subscription`]), so an
    /// acceptance and a rejection can't both go out (Paykit spec 8.2).
    ///
    /// While sends to the payee are held (Paykit spec 8.1, "Blocked counterparties"): while
    /// the copy of the deny list holds it, or while that copy isn't known, which a read of the
    /// lists is tried for first, the rejection is recorded all the same, on the same
    /// conditions, so no own app pays or accepts the request, and only its message is held:
    /// nothing is queued, nothing goes to the payee, and the message is sent at the first
    /// receive after sends to the payee aren't held. One held past its replay window, 30 days
    /// from this call, is never sent, and a [`crate::NoticeKind::MessageGivenUp`] notice
    /// tells the host; the record still holds it.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`];
    /// [`Refusal::Rejected`], [`Refusal::Cancelled`],
    /// [`Refusal::AlreadyPaid`] or [`Refusal::PaymentInProgress`]; [`Refusal::Subscribed`]
    /// for a recurring request already accepted; [`Refusal::DeadlinePassed`] for a recurring
    /// request whose proposal expired, decided once the record the service holds shows it
    /// still unanswered: one another own app accepted meanwhile gives
    /// [`Refusal::Subscribed`], one rejected or cancelled gives that refusal;
    /// [`Refusal::Usage`] for a one-time request the record shows accepted, by this app or
    /// another own app, which is cancelled instead, also once the acceptance's payment failed
    /// (Paykit spec 8.1, one response per request); [`PaykitError::Conflict`] when the record
    /// changed meanwhile, unless it now shows the request answered, which gives what answered
    /// it; the client's errors.
    pub async fn reject_request(
        &mut self,
        request: &RequestRef,
        reason: Option<String>,
    ) -> Result<String> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        // The answer's future goes on the heap, as `receive`'s does: it nests deep.
        let (msg_id, _) = Box::pin(self.answer_request(
            request,
            Answer::Reject,
            reason,
            AcceptOptions::default(),
            None,
        ))
        .await?;
        Ok(msg_id)
    }

    /// The payer cancels a request (Paykit spec 8.1, 8.2; Paykit wire 4.8), recorded and sent
    /// as a rejection is, and held as a rejection is: while sends to the payee are held it is
    /// recorded, and its message goes once they aren't. A one-time request's cancellation is
    /// conditioned on its claim; a recurring request's, which ends the subscription, on the
    /// claim of every Billing Period its record lists in `attempted_periods`, each at the
    /// step read with no open or `unknown` attempt, and on the record's version: an attempt
    /// on a period not listed writes the record, so an attempt and the cancellation can't
    /// both succeed. Returns the cancellation's `msg_id`.
    ///
    /// # Errors
    ///
    /// As [`Paykit::reject_request`]; for a recurring request, [`Refusal::PaymentInProgress`]
    /// while a listed period's attempt is open, and [`Refusal::MayBePaid`] while one's
    /// outcome is `unknown`.
    pub async fn cancel_request(
        &mut self,
        request: &RequestRef,
        reason: Option<String>,
    ) -> Result<String> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        // The answer's future goes on the heap, as `receive`'s does: it nests deep.
        let (msg_id, _) = Box::pin(self.answer_request(
            request,
            Answer::Cancel,
            reason,
            AcceptOptions::default(),
            None,
        ))
        .await?;
        Ok(msg_id)
    }

    /// The user accepts the subscription `request`, a recurring request (Paykit spec 8.2,
    /// Paykit wire 4.6): the acceptance is recorded in the request record, in a batch
    /// conditioned on the version of the record read and, when the request has a proposal
    /// expiry, on `server_time_in(null, proposal_expires_at + 1 ms)`, its own millisecond
    /// the last at which it is recorded; it is sent once that batch is applied. A rejection
    /// is recorded the same way, so the two can't both go out. Accepting pays nothing: each
    /// Billing Period is paid on its own. A subscription whose last Billing Period has ended
    /// is accepted too, as in v0.1, and shows [`RequestStatus::Ended`].
    ///
    /// It takes paying's guards (Paykit spec 7.2, 8.1): a request held after a restore is
    /// accepted only once the user has seen the payee may have cancelled it, and a blocked
    /// payee's only once the user confirms it ([`Paykit::accept_subscription_with`]). The
    /// deny list is judged as for a payment the user starts: as read at most 10 minutes
    /// before, else read first, and a read that fails is the call's error, so nothing is
    /// recorded or sent on a copy of the lists not known, a confirmation in hand included.
    ///
    /// # Errors
    ///
    /// [`Refusal::SwitchUnavailable`], or the client's error, when the deny list has to be
    /// read and can't be; [`Refusal::NotFound`]; [`Refusal::Usage`] for a one-time request,
    /// which is accepted by paying it; [`Refusal::Subscribed`] when it is accepted already,
    /// here or by another own app, also when the record changed meanwhile to show that
    /// acceptance; [`Refusal::HeldAfterRestore`]; [`Refusal::Blocked`]; otherwise as
    /// [`Paykit::reject_request`]. An acceptance is never recorded and held as a rejection
    /// is while sends to the payee are held: it takes paying's gate above. A failure that leaves the acceptance's batch kept (the
    /// network, `429`, `503`) is no error: it is reported sent, [`MessageDelivery::Kept`], so
    /// the host doesn't accept again.
    pub async fn accept_subscription(&mut self, request: &RequestRef) -> Result<SentMessage> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.accept_subscription_with(request, AcceptOptions::default())
            .await
    }

    /// [`Paykit::accept_subscription`], with what the user confirmed: that the payee may
    /// have cancelled a request held after a restore, or the subscription of a blocked
    /// payee, as the refusal showed it.
    ///
    /// # Errors
    ///
    /// As [`Paykit::accept_subscription`].
    pub async fn accept_subscription_with(
        &mut self,
        request: &RequestRef,
        options: AcceptOptions,
    ) -> Result<SentMessage> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.accept_subscription_as(request, options, None).await
    }

    /// Accept a recurring request under this wallet's approved Allowance (Paykit spec
    /// 8.2, 8.3). The signed acceptance and the selected Allowance are recorded together.
    /// Each Billing Period still requires its own eligible payment and available budget.
    ///
    /// # Errors
    ///
    /// As [`Paykit::accept_subscription`]. A missing grant, a held request or terms that
    /// do not cover the request refuse automatic acceptance.
    pub async fn accept_subscription_under_allowance(
        &mut self,
        request: &RequestRef,
        allowance: &[u8; 32],
    ) -> Result<SentMessage> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.accept_subscription_as(request, AcceptOptions::default(), Some(allowance))
            .await
    }

    async fn accept_subscription_as(
        &mut self,
        request: &RequestRef,
        options: AcceptOptions,
        allowance: Option<&[u8; 32]>,
    ) -> Result<SentMessage> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        // The answer's future goes on the heap, as `receive`'s does: it nests deep.
        let (msg_id, delivery) =
            Box::pin(self.answer_request(request, Answer::Accept, None, options, allowance))
                .await?;
        // An acceptance is never held: it is recorded and queued, or refused.
        let delivery = delivery.ok_or_else(PaykitError::corrupt)?;
        Ok(SentMessage { msg_id, delivery })
    }

    async fn answer_request(
        &mut self,
        request: &RequestRef,
        answer: Answer,
        reason: Option<String>,
        options: AcceptOptions,
        allowance: Option<&[u8; 32]>,
    ) -> Result<(String, Option<MessageDelivery>)> {
        self.require_pay()?;
        // An answer goes to no counterparty whose sends are held (Paykit spec 8.1, "Blocked
        // counterparties"). A rejection or a payer's cancellation is still a stop: it is
        // recorded as when they aren't, and only its message waits ([`Paykit::record_held`]);
        // a copy of the deny list past its refresh interval or not known is read first. An acceptance takes paying's gate
        // instead (Paykit spec 8.1, 8.2): the deny list as judged for a payment the user
        // starts, read first when the copy is older than 10 minutes or not known, a read that
        // fails being the call's error, so it never goes on a copy not known; to a blocked
        // payee it goes only on the user's confirmation, below. It is never held.
        let (blocked, held) = if allowance.is_some() {
            self.refuse_while_payer_pass_owed().await?;
            self.app.may_attempt(true).map_err(crate::pay::held_back)?;
            (self.denied(&request.payee).await?, false)
        } else if answer == Answer::Accept {
            (self.denied_for_user(&request.payee).await?, false)
        } else {
            // Judged on a fresh copy, as its delivery is: one past its refresh interval is
            // read first, so a block another own app wrote holds the answer in its record,
            // not the outbox, and so does a copy the read leaves stale or not known.
            self.refresh_lists_when_due().await?;
            (false, self.delivery_held(&request.payee).await?)
        };
        let (target, mut record) = self.payer_request(request).await?;
        let (_, body) = request_of(&record).ok_or(Refusal::NotFound)?;
        let recurring = body.is_recurring();
        let label = body.label.clone();
        let acceptance_end = body.acceptance_end();
        let stopped = self.kept_stop(&target).await?;
        self.refuse_answer(&record, body, answer, stopped)?;
        // An acceptance takes paying's guards: held after a restore, it waits for the user
        // to see the payee may have cancelled it, which ends the hold (Paykit spec 7.2); a
        // blocked payee's waits for the user's confirmation (Paykit spec 8.1).
        let mut end_hold = None;
        if answer == Answer::Accept {
            let record_key = self.activity_key(&target)?;
            // An acceptance by hand of a request whose record is paused as divergent, or whose
            // kept messages wait, waits for the user's confirmation: the payee may have
            // cancelled it (Paykit spec 6). Refused naming the record, nothing recorded.
            Box::pin(self.refuse_while_paused(&[record_key])).await?;
            end_hold = self
                .pass_hold(
                    &record_key,
                    &record,
                    options.possibly_cancelled_confirmed.as_ref(),
                )
                .await?;
            let shown = BlockedPayment {
                payee: request.payee,
                request: Some(*request),
                period: None,
                amount: body.amount.clone(),
            };
            crate::pay::refuse_blocked(blocked, shown, options.blocked_confirmed.as_ref())?;
        }
        let expiry = expiry_deadline(&record, body);
        // Past its proposal expiry, a recurring request can't be accepted or rejected: the
        // batch's time condition would refuse it on every try. The copy held may be stale, so
        // the record the switch holds decides. Its cancellation carries no time condition.
        let timed = recurring && answer != Answer::Cancel;
        if timed && self.reached(expiry) {
            return Err(self.expired_answer(&target, answer, None).await?);
        }
        let funding = if let Some(key) = allowance {
            let fresh = self.current_period_activity(&target, None).await?;
            let request_key = self.activity_key(&target)?;
            let held = self.app.shared_record(&request_key).await?;
            if !matches!(fresh, Some(ref current) if current.to_json()? == record.to_json()?)
                || !held.is_some_and(|r| r.authenticated && !r.doubtful)
                || self.pause_at(&request_key).await?.blocks_automatic()
            {
                return Err(Refusal::NotNow(
                    "the current subscription is not available for automatic acceptance".into(),
                )
                .into());
            }
            let conditions = self.app.record_condition(&request_key).await?;
            let reservation = Box::pin(self.allowance_reservation(key, &request.payee)).await?;
            Box::pin(reservation.subscription(self, request, &record)).await?;
            Some((reservation, conditions))
        } else {
            None
        };
        let obligation = self.obligation(&request.payee, &request.request_hash, 0);
        if !recurring {
            if let Some(read) = self.app.read_claim(&obligation).await? {
                refuse_on_claim(&read.view)?;
            }
        }
        let (kind, role, body) = match answer {
            Answer::Accept => (
                Kind::PaymentRequestAcceptance,
                Role::Response,
                PaymentRequestAcceptance { label }.to_body()?,
            ),
            Answer::Reject => (
                Kind::PaymentRequestRejection,
                Role::Response,
                RequestReply { reason, label }.to_body()?,
            ),
            Answer::Cancel => (
                Kind::PaymentRequestCancellation,
                Role::Cancellation,
                RequestReply { reason, label }.to_body()?,
            ),
        };
        let signed = self.sign(request.payee, kind, vec![request.request_hash], body)?;
        let msg_id = signed.msg_id().to_owned();
        let mut tx = Tx::default();
        if let Some(end) = end_hold {
            // The hold ends with the acceptance, and stays when it isn't recorded.
            tx.put(&end.confirmation(), &true)?;
        }
        if answer == Answer::Accept && blocked {
            // The user confirmed accepting this blocked payee's subscription: its original
            // delivery may go while sends to the payee are held, written with the response
            // and its queued message (Paykit spec 8.1, 8.2).
            tx.put(
                &key::send_exemption(signed.msg_id()),
                &crate::send_holds::SendExemption::confirmed_while_blocked(
                    signed.msg_id(),
                    request.payee,
                    request.request_hash,
                ),
            )?;
        }
        if !held {
            // A held answer queued keeps what ends its outbox copy past its 30 days, in the
            // commit that queues it (Paykit spec 8.1).
            let kind = if answer == Answer::Accept {
                crate::send_holds::HeldKind::SubscriptionAcceptance
            } else {
                crate::send_holds::HeldKind::Stop
            };
            tx.put(
                &key::queued_stop(signed.msg_id()),
                &crate::send_holds::QueuedStop::signed(
                    &signed,
                    kind,
                    Some(request.request_hash),
                    Some(&target),
                )?,
            )?;
        }
        if answer == Answer::Cancel {
            tx.put(
                &key::side(signed.msg_id()),
                &KeptSide {
                    msg_id: signed.msg_id().to_owned(),
                    side: SideTag::Payer,
                    label: None,
                },
            )?;
        }
        // Recorded in the request record in the batch that queues it, on the version of the
        // record read. A one-time request's reply is also conditioned on the claim at the
        // step read: an attempt meanwhile fails the batch, and the reply goes nowhere (Paykit
        // spec 8.1). A recurring request has no claim before it is accepted; its acceptance
        // and rejection are conditioned on the proposal not having expired, when it has an
        // expiry, so the two can't both go out (Paykit spec 8.2). Its payer's cancellation is
        // conditioned on the claims of the Billing Periods its record lists instead
        // ([`Paykit::cancel_conditions`]).
        let conditions = if recurring && answer == Answer::Cancel {
            self.cancel_conditions(&record, request).await?
        } else if recurring {
            acceptance_end
                .map(|end| Condition::ServerTimeIn {
                    start: None,
                    end: Some(end),
                })
                .into_iter()
                .collect()
        } else {
            vec![self.app.claim_condition(&obligation).await?]
        };
        let written = if let Some((funding, original)) = funding {
            record.allowance_id = Some(funding.id.clone());
            let stored = self.own_copy(&signed)?;
            if !activity::place(&mut record, role, stored) {
                return Err(Refusal::Subscribed.into());
            }
            let mut conditions = conditions;
            conditions.extend([original, funding.grant()]);
            if let Some(active) = funding.active_window() {
                conditions.push(active);
            }
            let write = self
                .app
                .record_op(
                    &target.record_target(),
                    Some(&record.to_json()?),
                    conditions,
                )
                .await?;
            self.queue_activity_write(signed, &target, Some(write), tx.into_writes())
                .await
        } else if held {
            self.record_held(
                signed,
                &target,
                role,
                conditions,
                tx.into_writes(),
                request.request_hash,
            )
            .await
        } else {
            self.send_with_record(signed, &target, role, conditions, tx.into_writes())
                .await
        };
        match written {
            Ok(_) => {}
            // An acceptance kept to go at the next write or read is sent, as far as the
            // caller goes, so the host doesn't accept again.
            Err(error) if answer == Answer::Accept && error.kept_batch().is_some() => {
                let batch_id = error.kept_batch().ok_or_else(PaykitError::corrupt)?;
                return Ok((msg_id, Some(MessageDelivery::Kept { batch_id })));
            }
            Err(error) => {
                if let PaykitError::Client(client) = &error {
                    if let Some(view) = claim_conflict(client.unkept()) {
                        if !recurring {
                            refuse_on_claim(&view)?;
                        }
                    }
                }
                // A listed period's claim moved since it was read: the claims, read again,
                // refuse the cancellation as the check before it does, an open attempt as in
                // progress and an `unknown` one as may be paid; claims that show no open
                // attempt leave a conflict, for the caller to read again (Paykit spec 8.2,
                // "Ending").
                if recurring
                    && answer == Answer::Cancel
                    && error.code() == Some(ErrorCode::Conflict)
                {
                    self.cancel_conditions(&record, request).await?;
                }
                // The proposal expired at the service: the answer's verified time says so.
                // Only on `409 conflict`: any other failure is the error. The record the
                // service holds decides, read again, and a record that isn't there or doesn't
                // read as this request leaves the conflict as the answer.
                if timed && error.code() == Some(ErrorCode::Conflict) && self.reached(expiry) {
                    let error = conflict(error);
                    return Err(self.expired_answer(&target, answer, Some(error)).await?);
                }
                // The record changed meanwhile, read again on the conflict: one that now
                // shows the request answered, here or by another own app, gives what
                // answered it, such as [`Refusal::Subscribed`].
                if error.code() == Some(ErrorCode::Conflict) {
                    if let Some(fresh) = self.held_activity(&target).await? {
                        if let Some((_, body)) = request_of(&fresh) {
                            self.refuse_answer(&fresh, body, answer, false)?;
                        }
                    }
                }
                return Err(conflict(error));
            }
        }
        // A held answer is recorded and not queued: nothing is delivered now.
        if held {
            return Ok((msg_id, None));
        }
        // An acceptance recorded and queued is sent whatever a try to deliver it now gives;
        // a rejection's or cancellation's failed try is the call's error, as before.
        if answer != Answer::Accept {
            // Its delivery is judged on a fresh copy of the deny list, which the time spent
            // recording it may have aged (Paykit spec 8.1, "Messages already queued").
            self.refresh_lists_when_due().await?;
            self.send_message(&msg_id).await?;
        }
        Ok((msg_id.clone(), Some(self.delivered(&msg_id).await?)))
    }
}

impl Paykit {
    /// The conditions of a payer's cancellation of the recurring request in `record`
    /// (Paykit spec 8.2, "Ending"): a `claim_step` on the claim of every Billing Period it
    /// lists in `attempted_periods`, at the step read, none of which may have an open or
    /// `unknown` attempt; the record's version is the write's own.
    ///
    /// # Errors
    ///
    /// [`Refusal::PaymentInProgress`] for a listed period with an open attempt;
    /// [`Refusal::MayBePaid`] for one whose attempt's outcome is `unknown`; the client's
    /// errors.
    async fn cancel_conditions(
        &mut self,
        record: &ActivityRecord,
        request: &RequestRef,
    ) -> Result<Vec<Condition>> {
        let (finished, conditions) = self.finished_periods(record, request).await?;
        for listed in &record.attempted_periods {
            if finished.contains(listed) {
                continue;
            }
            // An `unknown` attempt may have paid, whatever the claim's state: may be paid,
            // until its owner reports it or another own app writes it `failed`.
            let claim_key = self.obligation(&request.payee, &request.request_hash, *listed);
            let unknown = self.app.read_claim(&claim_key).await?.is_some_and(|read| {
                read.view
                    .attempts
                    .iter()
                    .any(|a| a.outcome == Outcome::Unknown)
            });
            if unknown {
                return Err(Refusal::MayBePaid {
                    reason: "Paykit spec 8.2: a listed period's attempt is unknown".into(),
                    doubt: None,
                }
                .into());
            }
            return Err(Refusal::PaymentInProgress.into());
        }
        Ok(conditions)
    }
}

/// What a payer answers a request with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// An acceptance, of a recurring request (Paykit spec 8.2).
    Accept,
    /// A rejection.
    Reject,
    /// A payer's cancellation.
    Cancel,
}

/// What the payee's other records say of a settlement ([`Paykit::other_credit`]).
enum OtherCredit {
    /// No other record credits it.
    None,
    /// Another record credits it.
    Credited,
    /// Whether one does can't be told now.
    CantTell,
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
pub(crate) fn expiry_deadline(record: &ActivityRecord, request: &PaymentRequest) -> Option<u64> {
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
    let status = if displayed_receipt(record, request.map(|(_, body)| body)).is_some() {
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
        divergent: false,
        recurring: request.is_some_and(|(_, r)| r.is_recurring()),
        periods: Vec::new(),
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
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
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

#[cfg(test)]
mod stop_tests {
    use paykit_lib::bodies::RequestReply;
    use paykit_lib::{
        Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    };
    use pubky_switch_client::clock::Clock;

    use super::*;
    use crate::testkit::Switches;

    /// A cancellation kept as the stop of a request record that reads, as the receive step
    /// keeps it before the later write judges it: the request's view counts it only when its
    /// sender may cancel that request (Paykit spec 3.4).
    #[tokio::test]
    async fn test_requests_view_counts_no_stop_from_another_label() {
        let w = Switches::new();
        let alice = w.alice.identity().public();
        let bob = w.bob.identity().public();
        let mut wallet = w
            .join(
                &w.alice,
                "wallet",
                crate::AppScopes::new().pay().build().unwrap(),
            )
            .await;
        wallet.app_mut().invite(&bob).await.unwrap();
        let mut shop1 = w.labelled("shop1", "shop1").await;
        let shop2 = w.labelled("shop2", "shop2").await;
        let mut endpoints = std::collections::BTreeMap::new();
        endpoints.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
        );
        let amount = PaymentAmount {
            value: Decimal::new("1").unwrap(),
            asset: Asset::new("btc").unwrap(),
        };
        // shop2's cancellation of shop1's request, then shop1's own.
        for stops in [false, true] {
            let draft = RequestDraft::new(
                amount.clone(),
                PaymentReference::new("order-1").unwrap(),
                RequestDestination::Endpoints(endpoints.clone()),
            );
            let request = shop1
                .send_request(alice, draft)
                .await
                .unwrap()
                .request
                .unwrap();
            wallet.receive(None).await.unwrap();
            let view = wallet.request(&request).await.unwrap();
            assert_eq!(view.status, RequestStatus::Open);
            let body = RequestReply {
                reason: None,
                label: None,
            }
            .to_body()
            .unwrap();
            let canceller = if stops { &shop1 } else { &shop2 };
            let signed = canceller
                .sign(
                    alice,
                    Kind::PaymentRequestCancellation,
                    vec![request.request_hash],
                    body,
                )
                .unwrap();
            // As alice's inbox took it: the stored copy of the received message.
            let message = pubky_switch_core::message::Message::parse(
                signed.plaintext().json(),
                paykit_lib::kinds::OPAQUE_MEMBERS,
            )
            .unwrap();
            let stop = StoredMessage::from_received(
                bob,
                canceller.app.cert().clone(),
                signed.plaintext().clone(),
                message,
                Some(w.clock.now_ms()),
            )
            .unwrap();
            let mut tx = Tx::default();
            tx.put(
                &key::kept_cancellation(&view.record, &stop.cert().hash(), 1),
                &KeptMessage::taken_at(
                    &stop,
                    FeedPosition {
                        generation: 1,
                        seq: 1,
                    },
                ),
            )
            .unwrap();
            wallet.local_commit(tx).await.unwrap();
            let status = wallet.request(&request).await.unwrap().status;
            let expected = if stops {
                RequestStatus::Cancelled
            } else {
                RequestStatus::Open
            };
            assert_eq!(status, expected);
        }
    }
}

#[cfg(test)]
mod stop_order_tests {
    use paykit_lib::bodies::RequestReply;
    use paykit_lib::{
        Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    };
    use pubky_switch_client::clock::Clock;

    use super::*;
    use crate::testkit::Switches;

    /// `canceller`'s cancellation of bob's request `request` to alice, as alice's inbox took it
    /// at `accepted_at`.
    fn stop_from(
        canceller: &Paykit,
        alice: [u8; 32],
        bob: [u8; 32],
        request: [u8; 32],
        accepted_at: u64,
    ) -> StoredMessage {
        let body = RequestReply {
            reason: None,
            label: None,
        }
        .to_body()
        .unwrap();
        let signed = canceller
            .sign(alice, Kind::PaymentRequestCancellation, vec![request], body)
            .unwrap();
        let message = pubky_switch_core::message::Message::parse(
            signed.plaintext().json(),
            paykit_lib::kinds::OPAQUE_MEMBERS,
        )
        .unwrap();
        StoredMessage::from_received(
            bob,
            canceller.app.cert().clone(),
            signed.plaintext().clone(),
            message,
            Some(accepted_at),
        )
        .unwrap()
    }

    /// Two valid stops of one request, from two of the payee's apps, accepted in the same
    /// millisecond: the one the inbox took first, by its intake `seq`, is placed, whichever
    /// app's key sorts first (Paykit spec 8.1: kept messages in inbox acceptance order).
    #[tokio::test]
    async fn test_requests_stops_in_one_millisecond_placed_in_intake_order() {
        let w = Switches::new();
        let alice = w.alice.identity().public();
        let bob = w.bob.identity().public();
        let mut wallet = w
            .join(
                &w.alice,
                "wallet",
                crate::AppScopes::new().pay().build().unwrap(),
            )
            .await;
        wallet.app_mut().invite(&bob).await.unwrap();
        let mut shop1 = w.labelled("shop1", "shop1").await;
        let shop = w
            .join(
                &w.bob,
                "shop",
                crate::AppScopes::new().request().build().unwrap(),
            )
            .await;
        let mut endpoints = std::collections::BTreeMap::new();
        endpoints.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
        );
        let draft = RequestDraft::new(
            PaymentAmount {
                value: Decimal::new("1").unwrap(),
                asset: Asset::new("btc").unwrap(),
            },
            PaymentReference::new("order-1").unwrap(),
            RequestDestination::Endpoints(endpoints),
        );
        let request = shop1
            .send_request(alice, draft)
            .await
            .unwrap()
            .request
            .unwrap();
        wallet.receive(None).await.unwrap();
        let record = wallet.request(&request).await.unwrap().record;
        let at = w.clock.now_ms();
        let stops = [
            (
                shop1.app.cert().hash(),
                stop_from(&shop1, alice, bob, request.request_hash, at),
            ),
            (
                shop.app.cert().hash(),
                stop_from(&shop, alice, bob, request.request_hash, at),
            ),
        ];
        // The app whose key sorts last gets the earlier intake: the key order alone would
        // pick the other one.
        let keys: Vec<String> = stops
            .iter()
            .map(|(cert, _)| key::kept_cancellation(&record, cert, 1))
            .collect();
        let first = usize::from(keys[0] < keys[1]);
        let mut tx = Tx::default();
        for (i, (_, stop)) in stops.iter().enumerate() {
            let seq = if i == first { 1 } else { 2 };
            tx.put(
                &keys[i],
                &KeptMessage::taken_at(stop, FeedPosition { generation: 1, seq }),
            )
            .unwrap();
        }
        wallet.local_commit(tx).await.unwrap();
        wallet.place_kept_cancellations().await.unwrap();
        let target =
            ActivityTarget::payer(natural_key::request_record(&bob, &request.request_hash));
        let held = wallet.held_activity(&target).await.unwrap().unwrap();
        let placed = held.cancellation.unwrap();
        assert_eq!(
            placed.message().msg_id,
            stops[first].1.message().msg_id,
            "the stop the inbox took first is placed"
        );
        assert_eq!(
            wallet.request(&request).await.unwrap().status,
            RequestStatus::Cancelled
        );
    }
}
