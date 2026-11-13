//! Paying (Paykit spec 7.1, 7.2, 8.1, 8.4, 8.5): the paying order, with the payment journal.
//!
//! 1. **Attempt.** The transfer is checked first, then, before any write, the own
//!    `identity.json` (read again, and confirmed within the last 60 s) and the payee's; the
//!    payment is journaled; then the `attempt`
//!    is written on the obligation's claim, conditioned on the version of the request record
//!    the app read and carrying the request's deadline. A payment without a request writes
//!    its `payer_activity` record in the same batch, and a single-use endpoint its
//!    `payment_lists` mark. The client checks the attempt's proof: only a verified answer
//!    showing this app's own open attempt lets money move.
//! 2. **Check, then pay.** The own `identity.json` is still confirmed and the destination checked
//!    ([`crate::destination`]); a failure writes `failed` instead of paying. The start of the
//!    payment is journaled, then the rail pays with the attempt ID as its idempotency key.
//!    A one-time request's acceptance, signed and journaled before, goes into the outbox
//!    only once that call is made, so a payee never sees one for a payment that never ran.
//! 3. **Outcome.** `done` (basis `owner`) or `failed` on the claim and the payer's activity
//!    record, with the acceptance and the proof, in one batch, which also queues the proof
//!    and finishes the journal entry once applied.
//! 4. **Proof**, to the payee, sent once that batch is applied.
//!
//! Every step commits before the next acts, so [`Paykit::resume_payments`] finishes a
//! payment a crash interrupted without paying twice (Paykit spec 7.2, "Starting to pay", "No
//! second execution").

use paykit_lib::bodies::{
    PaymentProof, PaymentRequestAcceptance, PrivatePaymentListRequest, RequestDestination,
};
use paykit_lib::derive::{natural_key, payment_obligation};
use paykit_lib::kinds::Kind;
use paykit_lib::records::{
    ActivityRecord, ActivitySide, DestinationSource, PaymentListRecord, UsedEndpoint,
};
use paykit_lib::{
    AttemptId, Body, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
};
use pubky_switch_client::app::{
    claim_conflict, AppError, ClaimAttempt, ClaimOp, Initiator, KeptBatch, KeptBatchStatus,
    LocalWrite, ObligationDeadline, OpBatch, PeerIdentity, RecordContent, Refusal as ClientRefusal,
    SignedMessage, UserConfirmed, REVALIDATION_MS,
};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::claims::{
    Basis, ClaimStatus, ClaimView, Outcome, MAX_CLAIM_AGE_MS, RECONCILIATION_MS,
};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::{Condition, OpKind};
use pubky_switch_core::records::{CLOCK_TOLERANCE_MS, DAY_MS};

use crate::activity::{self, ActivityTarget, Role};
use crate::destination::{Carrier, Need};
use crate::error::{PaykitError, Refusal, Result};
use crate::journal::{Destination, JournalEntry, KeptAmount, PaymentView, Stage, TransferBound};
use crate::notices::NoticeKind;
use crate::paykit::Paykit;
use crate::rail::{
    CandidateId, PaymentEndpointCandidate, RailError, RailOutcome, RailPaid, RailPayment, Transfer,
};
use crate::records::{list_target, KeptMessage};
use crate::requests::{cancelled, rejected, request_of, unreadable_request, Fresh, RequestRef};
use crate::state::{self, key};

/// How long after the latest time it has seen a payment without a request's attempt pins its
/// deadline: 10 minutes (Paykit spec 8.4).
pub const UNREQUESTED_DEADLINE_MS: u64 = 10 * 60_000;

/// Private or public payment, chosen explicitly (Paykit spec 8.4): a payer never changes from
/// one to the other on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum PaymentMode {
    /// A Private Payment List endpoint.
    #[default]
    Private,
    /// The payee's public endpoints, chosen by the user.
    Public(UserConfirmed),
}

/// A manual payment's choices.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PayOptions {
    /// The Payment Endpoint Identifier to pay; the first usable one when `None`.
    pub endpoint: Option<PaymentEndpointIdentifier>,
    /// The payee app whose Private Payment List to pay, for a destination that isn't the
    /// request's own; any current one when `None`.
    pub list_app: Option<[u8; 16]>,
    /// Private or public, for a destination that isn't the request's own.
    pub mode: PaymentMode,
    /// The doubt the user was shown and chose to pay over, as a [`Refusal::MayBePaid`]
    /// gave it (Paykit spec 7.1, 7.2). Only that same doubt is passed: the SDK reads the
    /// claim again, and a doubt that changed since, such as another own app's attempt turned
    /// `unknown` or flagged `possibly_duplicate`, is refused again with the new one.
    pub may_be_paid_confirmed: Option<PaymentDoubt>,
    /// The transfer the user was shown and chose to pay anyway, as a refusal for another
    /// amount, or a fee above the app's cap, gave it ([`Refusal::Transfer`]; Paykit spec 7.2
    /// step 1). It lets only that destination, with that same transfer, be paid: one chosen
    /// or quoted differently since is refused again with what it would transfer now.
    ///
    /// Consent is needed only for what the user didn't ask for. A destination whose
    /// transfer passes the check, the amount asked for with no fee beyond the cap, is paid
    /// without it, on a call carrying a confirmation too: the user asked for that payment
    /// when they called pay, as in v0.1, where the wallet paid the target it resolved. The
    /// confirmed quote is then not used.
    pub transfer_confirmed: Option<TransferQuote>,
    /// The user has seen that the payee's switch couldn't confirm the payee's apps, and pays
    /// anyway ([`Refusal::IdentityUnconfirmed`]; Switch spec 4.3, Paykit spec 7.2).
    pub unconfirmed_identity_confirmed: Option<UserConfirmed>,
    /// The user has seen that the payee may have cancelled this request since the backup the
    /// app was restored from, or a `missed` item, and pays it anyway
    /// ([`Refusal::HeldAfterRestore`]; Paykit spec 7.2, "What a restore loses"). It ends the
    /// request's hold; it never overrides a cancellation the app holds.
    pub possibly_cancelled_confirmed: Option<UserConfirmed>,
    /// The payment of a request from a blocked payee the user was shown and chose to make,
    /// as a [`Refusal::Blocked`] gave it (Paykit spec 8.1): only that same payment is made.
    pub blocked_confirmed: Option<BlockedPayment>,
}

impl PayOptions {
    /// The Payment Endpoint Identifier to pay; the first usable one when not set.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: PaymentEndpointIdentifier) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// The payee app whose Private Payment List to pay, for a destination that isn't the
    /// request's own; any current one when not set.
    #[must_use]
    pub fn with_list_app(mut self, list_app: [u8; 16]) -> Self {
        self.list_app = Some(list_app);
        self
    }

    /// Private or public, for a destination that isn't the request's own.
    #[must_use]
    pub fn with_mode(mut self, mode: PaymentMode) -> Self {
        self.mode = mode;
        self
    }

    /// The doubt the user was shown and chose to pay over (Paykit spec 7.1, 7.2): only that
    /// same doubt is passed.
    #[must_use]
    pub fn with_may_be_paid_confirmed(mut self, doubt: PaymentDoubt) -> Self {
        self.may_be_paid_confirmed = Some(doubt);
        self
    }

    /// The transfer the user was shown and chose to pay anyway, as a refusal for another
    /// amount, or a fee above the app's cap, gave it ([`Refusal::Transfer`]; Paykit spec 7.2
    /// step 1). Only that destination, with that same transfer, is paid.
    #[must_use]
    pub fn with_transfer_confirmed(mut self, quote: TransferQuote) -> Self {
        self.transfer_confirmed = Some(quote);
        self
    }

    /// The user has seen that the payee's switch couldn't confirm the payee's apps, and pays
    /// anyway ([`Refusal::IdentityUnconfirmed`]; Switch spec 4.3, Paykit spec 7.2).
    #[must_use]
    pub fn with_unconfirmed_identity_confirmed(mut self, confirmed: UserConfirmed) -> Self {
        self.unconfirmed_identity_confirmed = Some(confirmed);
        self
    }

    /// The user has seen that the payee may have cancelled this request since the backup the
    /// app was restored from, and pays it anyway ([`Refusal::HeldAfterRestore`]; Paykit spec
    /// 7.2, "What a restore loses"). It ends the request's hold; it never overrides a
    /// cancellation the app holds.
    #[must_use]
    pub fn with_possibly_cancelled_confirmed(mut self, confirmed: UserConfirmed) -> Self {
        self.possibly_cancelled_confirmed = Some(confirmed);
        self
    }

    /// The payment of a request from a blocked payee the user was shown and chose to make,
    /// as a [`Refusal::Blocked`] gave it (Paykit spec 8.1): only that same payment is made.
    #[must_use]
    pub fn with_blocked_confirmed(mut self, payment: BlockedPayment) -> Self {
        self.blocked_confirmed = Some(payment);
        self
    }
}

/// A payment to a payee the identity's deny list holds, as a [`Refusal::Blocked`] shows it
/// to the user (Paykit spec 8.1, "Blocked counterparties"): the payee, the request paid, if
/// any, and the amount. A payment the user then makes anyway carries it back
/// ([`PayOptions::with_blocked_confirmed`]): the SDK judges the payment again, by the deny
/// list as read in the last 10 minutes or read first, as at the first try, and pays only
/// when what it would show now is this, field for field.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlockedPayment {
    /// The blocked payee.
    pub payee: [u8; 32],
    /// The request paid, `None` for a payment without one.
    pub request: Option<RequestRef>,
    /// The amount paid.
    pub amount: PaymentAmount,
}

/// Refuse a payment the user starts to a payee the deny list holds, `blocked`, unless the
/// user confirmed `shown`, the same payment (Paykit spec 8.1).
fn refuse_blocked(blocked: bool, shown: BlockedPayment, options: &PayOptions) -> Result<()> {
    if blocked && options.blocked_confirmed.as_ref() != Some(&shown) {
        return Err(Refusal::Blocked {
            payment: Box::new(shown),
        }
        .into());
    }
    Ok(())
}

/// Why a payment may already have been made, as a [`Refusal::MayBePaid`] shows it to the
/// user (Paykit spec 7.1, 7.2): the obligation, the claim's step read, the attempts that
/// make it doubtful by name, and the claim's flags. A payment the user then makes anyway
/// carries it back ([`PayOptions::with_may_be_paid_confirmed`]): the SDK reads the claim
/// again and goes on only when the doubt now is this one, field for field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PaymentDoubt {
    /// The claim key.
    pub obligation: [u8; 32],
    /// The claim's step read; `None` for no claim.
    pub step: Option<u64>,
    /// This app's attempts a restore left unresolved (Switch spec 12.4).
    pub restored: Vec<AttemptId>,
    /// Attempts marked `unknown`.
    pub unknown: Vec<AttemptId>,
    /// Open attempts orphaned now.
    pub orphaned: Vec<AttemptId>,
    /// The next attempt would be flagged `possibly_duplicate`.
    pub next_possibly_duplicate: bool,
    /// Uploads after a provider loss conflict.
    pub divergent: bool,
    /// An entry by an app the own `identity.json` lists as removed, after its
    /// `distrust_from`.
    pub after_distrust: bool,
    /// The pinned deadline is earlier than the app's own.
    pub early_pin: bool,
}

impl PaymentDoubt {
    /// Whether anything in it makes the payment doubtful.
    pub fn doubtful(&self) -> bool {
        !self.restored.is_empty()
            || !self.unknown.is_empty()
            || !self.orphaned.is_empty()
            || self.next_possibly_duplicate
            || self.divergent
            || self.after_distrust
            || self.early_pin
    }
}

/// What a destination would transfer, as a payment shows it to the user before paying it on
/// their explicit choice (Paykit spec 7.2 step 1): the destination, named by its payee app,
/// Payment Endpoint Identifier and a hash of its payload, never the payload itself; the
/// amount paid; and the whole [`Transfer`] the payment method reported. A
/// [`Refusal::Transfer`] carries it, and a payment the user then makes anyway carries it
/// back ([`PayOptions::with_transfer_confirmed`]): the SDK chooses and asks the method
/// again, and pays a destination failing the transfer check only when it and its transfer
/// are this one, field for field.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransferQuote {
    /// The payee.
    pub payee: [u8; 32],
    /// The payee app whose destination it is.
    pub app_id: [u8; 16],
    /// The Payment Endpoint Identifier.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// The hash of the Payment Endpoint Payload, which names it without showing it.
    pub payload_hash: [u8; 32],
    /// The amount paid: the request's, or the payment without a request's.
    pub amount: PaymentAmount,
    /// What the method said it would transfer: its amount and largest fee.
    pub transfer: Transfer,
}

/// The result of a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PayOutcome {
    /// Paid: `done` written, the proof queued.
    #[non_exhaustive]
    Paid {
        /// The attempt.
        attempt_id: AttemptId,
        /// The proof's `msg_id`.
        proof_msg_id: Option<String>,
    },
    /// Paid, but the outcome couldn't be written: the app isn't current any more. Its
    /// attempt is reconciled through the receipt or by another app (Paykit spec 7.3).
    #[non_exhaustive]
    PaidUnreported {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// Definitively not paid: `failed` written.
    #[non_exhaustive]
    Failed {
        /// The attempt.
        attempt_id: AttemptId,
        /// Why, without content.
        reason: String,
    },
    /// Nothing yet: the attempt stays open until the payment method reports (Paykit spec
    /// 7.2, "Failed means definitively not paid").
    #[non_exhaustive]
    Pending {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// The payment method's history can't tell: "may be paid", resolved only by a receipt or
    /// by the user (Paykit spec 7.2, 7.3).
    #[non_exhaustive]
    Uncertain {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// Paid by the method, but the outcome's batch didn't go through (the switch didn't
    /// answer, or the write failed otherwise). The journal keeps the proof and the attempt
    /// stays open, so nothing pays again; a resume ([`Paykit::resume_payments`]) writes the
    /// outcome and sends the proof (Paykit spec 7.2 steps 3 and 4).
    #[non_exhaustive]
    PaidOutcomeUnwritten {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// Paid by the method, but the outcome waits: the payer's activity record holds a value
    /// that doesn't read, and `done` never goes without its update (Paykit spec 7.2 step
    /// 3). Nothing was written; the attempt stays open, so nothing pays again, and the
    /// journal keeps the proof ([`PaymentView::record_held`]). It settles at a resume once
    /// the record reads again, or with [`Paykit::repair_activity_record`].
    #[non_exhaustive]
    RecordHeld {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// Paid by the method, but the outcome waits: the payer's activity record is divergent
    /// after a provider loss and the user hasn't confirmed it, and an own write over it
    /// that adds no stop waits for that (Paykit spec 6). Nothing was written; the attempt
    /// stays open, so nothing pays again, and the journal keeps the proof. Once the user
    /// confirms `record` ([`Paykit::confirm_divergent`]), a resume writes the outcome and
    /// sends the proof.
    #[non_exhaustive]
    RecordDivergent {
        /// The attempt.
        attempt_id: AttemptId,
        /// The payer's activity record.
        record: [u8; 32],
    },
}

impl PayOutcome {
    /// The attempt the outcome reports: every outcome names one.
    pub fn attempt_id(&self) -> AttemptId {
        match self {
            Self::Paid { attempt_id, .. }
            | Self::PaidUnreported { attempt_id, .. }
            | Self::Failed { attempt_id, .. }
            | Self::Pending { attempt_id, .. }
            | Self::Uncertain { attempt_id, .. }
            | Self::PaidOutcomeUnwritten { attempt_id, .. }
            | Self::RecordHeld { attempt_id, .. }
            | Self::RecordDivergent { attempt_id, .. } => *attempt_id,
        }
    }
}

/// A payment without a request (Paykit spec 8.4).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnrequestedPayment {
    /// The payee.
    pub payee: [u8; 32],
    /// The amount.
    pub amount: PaymentAmount,
    /// The choices.
    pub options: PayOptions,
}

impl UnrequestedPayment {
    /// A payment of `amount` to `payee`, with the default choices.
    pub fn new(payee: [u8; 32], amount: PaymentAmount) -> Self {
        Self {
            payee,
            amount,
            options: PayOptions::default(),
        }
    }

    /// The choices.
    #[must_use]
    pub fn with_options(mut self, options: PayOptions) -> Self {
        self.options = options;
        self
    }
}

/// What held a payment back at [`Paykit::pay_gate`].
pub(crate) enum Held {
    /// The own `identity.json` isn't confirmed in the last 60 s (check 4).
    OwnIdentity(Refusal),
    /// Any other check.
    Check(Refusal),
}

impl Held {
    fn refusal(self) -> Refusal {
        match self {
            Self::OwnIdentity(refusal) | Self::Check(refusal) => refusal,
        }
    }
}

/// What one attempt needs beyond its journal entry.
struct Plan {
    entry: JournalEntry,
    record: ActivityTarget,
    carrier: Carrier,
    step: u64,
    /// The `payment_lists` record whose single-use mark the attempt's batch writes.
    reserve: Option<PaymentListRecord>,
    /// The record a payment without a request creates in the attempt's batch.
    create: Option<ActivityRecord>,
    /// Condition the attempt on the version of the request record read.
    on_record: bool,
    /// Who starts the payment: the user, or an executor (Switch spec 4.3).
    by: Initiator,
    /// The user's confirmation of a payee identity its switch couldn't confirm.
    unconfirmed: Option<UserConfirmed>,
    /// The request record whose restore hold the user's confirmation ends, in the attempt's
    /// batch: a call refused before it keeps the hold.
    end_hold: Option<[u8; 32]>,
}

/// A Payment Endpoint a payment may take: its identifier and payload, handed to the payment
/// method as a candidate ([`crate::Rail::select_endpoints`]), and what paying it needs: the
/// destination journaled, its carrier, and, for a single-use list endpoint, the
/// `payment_lists` record whose mark the attempt's batch writes.
struct Choice {
    endpoint_id: PaymentEndpointIdentifier,
    payload: PaymentEndpointPayload,
    destination: Destination,
    carrier: Carrier,
    reserve: Option<PaymentListRecord>,
}

impl Choice {
    fn new(
        endpoint_id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        destination: Destination,
        carrier: Carrier,
        reserve: Option<PaymentListRecord>,
    ) -> Self {
        Self {
            endpoint_id: endpoint_id.clone(),
            payload: payload.clone(),
            destination,
            carrier,
            reserve,
        }
    }
}

/// The endpoint a payment takes, as [`Paykit::choose`] chose it.
type Chosen = (
    Destination,
    Carrier,
    Option<PaymentListRecord>,
    TransferBound,
);

/// The payment method's answer, as [`Paykit::rail_order`] checked it: the places of the
/// candidates it pays, in its order, and the first error of one it couldn't judge, by the
/// order handed in.
struct Ordered {
    payable: Vec<usize>,
    error: Option<RailError>,
}

/// Why the transfer check refuses `transfer` for `amount` (Paykit spec 7.2 step 1), under
/// the fee cap `cap`: another amount or asset, or one the method can't tell, or a fee above
/// the cap. A fee of nothing needs no cap: the default, with no caps, still pays it. The fee
/// is in the transfer's asset, which is the payment's (checked first). `None` when it passes.
fn transfer_refused(
    transfer: &Transfer,
    amount: &PaymentAmount,
    cap: Option<&paykit_lib::Decimal>,
) -> Option<&'static str> {
    if transfer.amount.as_ref() != Some(amount) {
        return Some(
            "the destination transfers another amount or asset, or one the method can't tell",
        );
    }
    transfer
        .max_fee
        .as_ref()
        .filter(|fee| !fee.is_zero())
        .is_some_and(|fee| cap.is_none_or(|cap| fee > cap))
        .then_some("a fee above the app's cap")
}

/// The refusal of a destination whose endpoints the payment method pays none of.
fn unsupported(what: &str) -> PaykitError {
    Refusal::UnsupportedEndpoint(format!(
        "Paykit spec 7.2: the payment method pays none of {what}"
    ))
    .into()
}

fn not_now(refusal: ClientRefusal) -> PaykitError {
    Refusal::NotNow(refusal.to_string()).into()
}

/// How old, on the device clock, the highest service time verified may be and still be
/// taken as the latest seen for a payment's deadline (Paykit spec 8.4): older, the switch is
/// read first.
const SERVICE_TIME_RECENT_MS: u64 = 60_000;

/// What [`Paykit::write_outcome`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Written {
    /// The outcome is written, with the record's update, or was already.
    Done,
    /// The outcome waits for the payer's activity record, which doesn't read: nothing was
    /// written ([`JournalEntry::record_held`]).
    Held,
    /// The outcome waits for the user's confirmation of the payer's activity record, which is
    /// divergent (Paykit spec 6): nothing was written.
    Divergent([u8; 32]),
}

/// The payer's activity record's part of an outcome's batch.
enum OutcomeRecord {
    /// This write.
    Write(Box<(Vec<Condition>, OpKind)>),
    /// None: the record already holds what the outcome places.
    Unchanged,
    /// The record can't be updated now: the outcome waits.
    Held,
    /// The record is divergent and the user hasn't confirmed it: the outcome waits.
    Divergent([u8; 32]),
}

/// When a first dispatch's attempt was answered: its service time, and the device's wall
/// and monotonic clocks just before it was sent, for its age at the final payment check and
/// the fence.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Answered {
    /// The attempt's `opened_at`, the service time its verified proof shows.
    pub(crate) opened_at_ms: u64,
    /// The device clock just before the attempt was sent: earlier than the service's
    /// acceptance, so the age counted from it is never short.
    pub(crate) wall_ms: u64,
    /// The monotonic clock just before the attempt was sent.
    pub(crate) at: std::time::Instant,
}

impl Answered {
    /// The attempt's age, in milliseconds, at device time `now`: the longer of the device
    /// clock's and the monotonic clock's elapsed time, so a clock set back can't hide time
    /// that passed, and the time a test clock moves counts.
    pub(crate) fn age_ms(&self, now: u64) -> u64 {
        let monotonic = u64::try_from(self.at.elapsed().as_millis()).unwrap_or(u64::MAX);
        now.saturating_sub(self.wall_ms).max(monotonic)
    }
}

/// Which dispatch [`Paykit::final_payment_check`] judges.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Dispatch {
    /// The first: nothing was submitted before.
    First {
        /// When its attempt was answered.
        answered: Answered,
    },
    /// A repeat under the same idempotency key, harmless only within the method's
    /// idempotency retention (Paykit spec 7.2), and made only while it can complete before
    /// its attempt is orphaned.
    Repeat {
        /// The retention the method declares; `None` for good.
        idempotency_retention_ms: Option<u64>,
        /// The attempt's `opened_at`, service time, from the claim read before the repeat.
        opened_at_ms: u64,
        /// The attempt's `orphan_after`, in seconds, from the same read.
        orphan_after_s: u64,
        /// The longest the method takes to complete or fail, as it declares.
        longest_completion_ms: u64,
        /// The highest service time verified once that read is answered, with the monotonic
        /// clock taken before it was sent: their sum later is never short of service time,
        /// the read's own time included, whatever the device clock was set to meanwhile.
        read_at: (u64, std::time::Instant),
    },
}

/// What [`Paykit::final_payment_check`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FinalCheck {
    /// Pay now.
    Pay,
    /// Not now: nothing is paid and the attempt stays open.
    Hold,
    /// A repeat past the idempotency retention, or one that might not complete before its
    /// attempt is orphaned: never made.
    Lapsed,
    /// The client refused the payee's late switch answer, such as an equivocation (Switch
    /// wire 3.3): nothing is paid, and the refusal is the caller's.
    Refused(Refusal),
}

/// Whether the payment method still keeps what it keeps for `retention` (`None` for good)
/// of a dispatch journaled at `journaled_at_ms`, at the device clock `now` (Paykit spec 7.2,
/// "What a payment method declares"): its age counts from the journaling, and the retention
/// is half-open, so at `journaled_at_ms + retention` the history or the idempotency key may
/// be gone already. A clock now behind the journaling can't tell the age, which is outside
/// every finite retention: an empty history proves nothing then, and a repeat may find the
/// key forgotten. A retention kept for good holds whatever the clock.
pub(crate) fn retained(retention: Option<u64>, journaled_at_ms: u64, now: u64) -> bool {
    retention.is_none_or(|r| now.checked_sub(journaled_at_ms).is_some_and(|age| age < r))
}

/// Whether `bound` has passed (Paykit spec 7.2, "Kept proofs"): it lies before the earlier
/// of the highest service time verified, `service`, and the device clock `now` plus 24
/// hours, so one far-future signed time can't pass it; without a verified service time
/// none has.
pub(crate) fn bound_passed(bound: u64, service: Option<u64>, now: u64) -> bool {
    service.is_some_and(|service| {
        bound < service.min(now.saturating_add(pubky_switch_core::records::DAY_MS))
    })
}

/// How many times the outcome's batch is built again after `409 conflict`.
const OUTCOME_TRIES: usize = 3;

/// The deadline the app computes itself for an obligation (Switch wire 6.8): the attempt's,
/// or none.
pub(crate) fn own_deadline(deadline: Option<u64>) -> ObligationDeadline {
    match deadline {
        Some(at) => ObligationDeadline::At(at),
        None => ObligationDeadline::NoDeadline,
    }
}

/// A `private_payment_list_request` asked of a payee's app (`lq/<payee>[/<app>]`, Paykit
/// spec 8.4): whom it asked, the version it named, and when. The payee and app repeat the
/// key's, so a restore binds the value to its key.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ListAsk {
    #[serde(with = "pubky_switch_core::enc::serde_b64")]
    pub(crate) payee: [u8; 32],
    pub(crate) app_id: Option<[u8; 16]>,
    pub(crate) held_version: Option<u64>,
    pub(crate) asked_at_ms: u64,
}

/// The local write that journals `entry` in a batch. The entry is checked first
/// ([`JournalEntry::check`]), the shape every read back checks before binding it to its key
/// (`state::Stored`), so no path persists a state recovery would refuse: one that fails is a
/// bug, refused as a corrupt store, with nothing written.
pub(crate) fn journal_write(entry: &JournalEntry) -> Result<LocalWrite> {
    entry.check()?;
    state::local_put(&key::journal(&entry.attempt()?), entry)
}

impl Paykit {
    /// Pay a one-time request by hand (Paykit spec 7.2, 8.1 step 3).
    ///
    /// # Errors
    ///
    /// A [`Refusal`] for a request that isn't payable: rejected, cancelled, paid, being paid
    /// by another own app, may already be paid, expired, recurring, without a usable
    /// destination, or a check before money moves that refused it: before the attempt was
    /// written, nothing was written but, for [`Refusal::WaitingForList`], the payee's list
    /// request (Paykit spec 8.4); after it, [`Refusal::AttemptFailed`] says `failed` was
    /// written; [`PaykitError::Conflict`] when the request record changed
    /// meanwhile; the client's errors. A failure after money may have moved is never an
    /// error: it is [`PayOutcome::Pending`] or [`PayOutcome::Uncertain`]. At an app holding
    /// `contacts`, the deny list as read at most 10 minutes ago, or read first, decides: a
    /// request from a blocked payee is [`Refusal::Blocked`] until the user confirms paying
    /// it.
    pub async fn pay_request(
        &mut self,
        request: &RequestRef,
        options: PayOptions,
    ) -> Result<PayOutcome> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.pay_request_inner(request, options)).await
    }

    async fn pay_request_inner(
        &mut self,
        request: &RequestRef,
        options: PayOptions,
    ) -> Result<PayOutcome> {
        self.refresh_own()?;
        self.records_known().await?;
        self.require_pay()?;
        self.app.may_attempt(false).map_err(not_now)?;
        self.refuse_while_payer_pass_owed().await?;
        let (target, record) = self.payer_request(request).await?;
        let (stored, body) = request_of(&record).ok_or(Refusal::NotFound)?;
        let (stored, body) = (stored.clone(), body.clone());
        if body.is_recurring() {
            return Err(Refusal::Recurring.into());
        }
        if record.shows_paid() {
            return Err(Refusal::AlreadyPaid.into());
        }
        if rejected(&record) {
            return Err(Refusal::Rejected.into());
        }
        // A payer that has recorded a cancellation, or keeps one not yet placed, makes no new
        // attempt (Paykit spec 8.1, the kept-copy stop).
        if cancelled(&record) || self.kept_stop(&target).await? {
            return Err(Refusal::Cancelled.into());
        }
        // Held after a restore: paid only once the user has seen the payee may have
        // cancelled it, which ends the hold (Paykit spec 7.2, "What a restore loses").
        let record_key = self.activity_key(&target)?;
        // Divergent after a provider loss, or seen so: paid by hand only once the user has
        // seen that the payee may have cancelled it (Paykit spec 6).
        self.refuse_while_paused(&[record_key]).await?;
        let end_hold = if self.held(&record_key, &record).await? {
            if options.possibly_cancelled_confirmed.is_none() {
                return Err(Refusal::HeldAfterRestore.into());
            }
            Some(record_key)
        } else {
            None
        };
        // A blocked payee's request is paid only once the user confirms it (Paykit spec 8.1).
        let blocked = self.denied_for_user(&request.payee).await?;
        let shown = BlockedPayment {
            payee: request.payee,
            request: Some(*request),
            amount: body.amount.clone(),
        };
        refuse_blocked(blocked, shown, &options)?;
        let obligation = self.obligation(&request.payee, &request.request_hash, 0);
        let deadline = body.attempt_deadline(None)?;
        let restored = self.restored_attempts(&obligation).await?;
        let step = self
            .claim_step(&obligation, deadline, restored, &options)
            .await?;
        // No attempt once the deadline is at or before the highest service time verified
        // (Paykit spec 7.1); the service refuses one from its own clock anyway. The claim is
        // read first: a payment made, or another wallet's open attempt, is what the request
        // shows, though its deadline has passed; then the request record, read again.
        if self.deadline_passed(deadline) {
            return Err(self.past_deadline(&target, None).await?);
        }
        let (destination, carrier, reserve, bound) = match &body.destination {
            RequestDestination::Endpoints(endpoints) => {
                let allowed = |id: &PaymentEndpointIdentifier| {
                    body.accepted_payment_endpoint_identifiers
                        .as_ref()
                        .is_none_or(|ids| ids.contains(id))
                };
                // The request's endpoints the payment may use, by identifier.
                let gathered: Vec<Choice> = endpoints
                    .iter()
                    .filter(|(id, _)| {
                        allowed(id) && options.endpoint.as_ref().is_none_or(|want| want == *id)
                    })
                    .map(|(id, payload)| {
                        let destination = Destination::new(
                            stored.message().from.app_id,
                            id,
                            payload,
                            DestinationSource::Request,
                            None,
                            false,
                        );
                        let carrier = Carrier::Message(Box::new(stored.clone()));
                        Choice::new(id, payload, destination, carrier, None)
                    })
                    .collect();
                if gathered.is_empty() {
                    return Err(Refusal::NoDestination(
                        "Paykit spec 8.1: the request has no such endpoint".into(),
                    )
                    .into());
                }
                self.choose(&request.payee, gathered, &body.amount, &options)?
                    .ok_or_else(|| unsupported("the request's endpoints"))?
            }
            // The request's accepted identifiers restrict a destination from the payee's lists
            // as they do its own endpoints (Paykit wire 4.5).
            RequestDestination::RequiredApp(app) => {
                self.list_destination(
                    &request.payee,
                    Some(*app),
                    body.accepted_payment_endpoint_identifiers.as_deref(),
                    &body.amount,
                    &options,
                )
                .await?
            }
            RequestDestination::Any => {
                self.list_destination(
                    &request.payee,
                    options.list_app,
                    body.accepted_payment_endpoint_identifiers.as_deref(),
                    &body.amount,
                    &options,
                )
                .await?
            }
        };
        let entry = JournalEntry {
            attempt_id: AttemptId::generate()?.to_text(),
            obligation,
            payee: request.payee,
            request_hash: Some(pubky_switch_core::enc::b64(&request.request_hash)),
            natural_key: target.natural_key.clone(),
            amount: KeptAmount::of(&body.amount),
            destination,
            deadline,
            stage: Stage::Recorded,
            step: None,
            label: body.label.as_ref().map(|l| l.as_str().to_owned()),
            payment_reference: Some(body.payment_reference.as_str().to_owned()),
            acceptance_owed: record.response.is_none(),
            acceptance_msg_id: None,
            acceptance_plaintext: None,
            acceptance_cert: None,
            acceptance_released: false,
            rail_proof: None,
            paid_at_ms: None,
            proof_msg_id: None,
            restored: false,
            journaled_at_ms: self.now(),
            carrier: None,
            carrier_record: None,
            attempt_proof: None,
            failed_by_method: false,
            request: Some(KeptMessage::of(&stored)),
            record_held: false,
            transfer_bound: Some(bound),
            failed_on_claim: false,
        };
        self.execute(Plan {
            entry,
            record: target,
            carrier,
            step,
            reserve,
            create: None,
            on_record: true,
            by: Initiator::User,
            unconfirmed: options.unconfirmed_identity_confirmed,
            end_hold,
        })
        .await
    }

    /// Pay without a request (Paykit spec 8.4): to the payee's Private Payment List, or its
    /// public endpoints when the user chose them. The attempt, its `payer_activity` record and
    /// a single-use endpoint's mark go in one batch, so two own apps can't both pay one
    /// single-use endpoint.
    ///
    /// # Errors
    ///
    /// As [`Paykit::pay_request`]; [`PaykitError::Conflict`] when the `payment_lists` record
    /// changed meanwhile, after which the caller pays to the next usable endpoint.
    pub async fn pay_without_request(&mut self, payment: UnrequestedPayment) -> Result<PayOutcome> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.pay_without_request_inner(payment)).await
    }

    async fn pay_without_request_inner(
        &mut self,
        payment: UnrequestedPayment,
    ) -> Result<PayOutcome> {
        self.refresh_own()?;
        self.records_known().await?;
        self.require_pay()?;
        self.app.may_attempt(false).map_err(not_now)?;
        self.refuse_while_payer_pass_owed().await?;
        let options = payment.options;
        // A payment to a blocked payee is made only once the user confirms it (Paykit spec
        // 8.1; v0.1 refused any payment to a blocked peer).
        let blocked = self.denied_for_user(&payment.payee).await?;
        let shown = BlockedPayment {
            payee: payment.payee,
            request: None,
            amount: payment.amount.clone(),
        };
        refuse_blocked(blocked, shown, &options)?;
        let (destination, carrier, reserve, bound) = self
            .list_destination(
                &payment.payee,
                options.list_app,
                None,
                &payment.amount,
                &options,
            )
            .await?;
        let attempt_id = AttemptId::generate()?;
        // The own switch is asked first, before anything is written: Paykit spec 8.4 lets
        // a payment to a reusable public endpoint go ahead while it doesn't answer, and this
        // SDK doesn't make one, every payment being attempt-first (Paykit spec 7.2). So a
        // switch that doesn't answer refuses the payment, nothing written or paid. The read
        // also makes the service time current.
        let obligation = payment_obligation(&attempt_id);
        self.app.read_claim(&obligation).await.map_err(|error| {
            if switch_unavailable(&error) {
                PaykitError::from(Refusal::SwitchUnavailable(error.to_string()))
            } else {
                error.into()
            }
        })?;
        // 10 minutes after the latest service time the app has seen (Paykit spec 8.4),
        // which a recent read makes current (Switch spec 10.1).
        let seen = self.recent_service_time(&obligation).await?;
        let own = self.identity();
        let natural = natural_key::payer_payment_record(&own, &attempt_id);
        let mut record = activity::empty(ActivitySide::Payer, payment.payee);
        record.attempt_id = Some(attempt_id);
        record.unrequested = Some(crate::receive::unrequested_of(
            &destination,
            &payment.amount,
        )?);
        record.status = Some("paying".into());
        let entry = JournalEntry {
            attempt_id: attempt_id.to_text(),
            obligation: payment_obligation(&attempt_id),
            payee: payment.payee,
            request_hash: None,
            natural_key: natural.clone(),
            amount: KeptAmount::of(&payment.amount),
            destination,
            deadline: Some(seen.saturating_add(UNREQUESTED_DEADLINE_MS)),
            stage: Stage::Recorded,
            step: None,
            label: None,
            payment_reference: None,
            acceptance_owed: false,
            acceptance_msg_id: None,
            acceptance_plaintext: None,
            acceptance_cert: None,
            acceptance_released: false,
            rail_proof: None,
            paid_at_ms: None,
            proof_msg_id: None,
            restored: false,
            journaled_at_ms: self.now(),
            carrier: None,
            carrier_record: None,
            attempt_proof: None,
            failed_by_method: false,
            request: None,
            record_held: false,
            transfer_bound: Some(bound),
            failed_on_claim: false,
        };
        self.execute(Plan {
            entry,
            record: ActivityTarget::payer(natural),
            carrier,
            step: 0,
            reserve,
            create: Some(record),
            on_record: false,
            by: Initiator::User,
            unconfirmed: options.unconfirmed_identity_confirmed,
            end_hold: None,
        })
        .await
    }

    /// The payments in the journal, each with the payee's receipt of it, if the payer's
    /// record holds one. A `Finished` or `Abandoned` payment leaves the journal once past its
    /// claim's window ([`Paykit::resume_payments`], "Journal pruning").
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn payments(&self) -> Result<Vec<PaymentView>> {
        let mut out = Vec::new();
        for entry in self.journal().await? {
            let mut view = entry.view()?;
            let target = ActivityTarget::payer(entry.natural_key.clone());
            if let Some(record) = self.held_activity(&target).await? {
                view.receipt = self
                    .received_receipt_of(self.activity_key(&target)?, &record)
                    .await?
                    .filter(|receipt| receipt.attempt_id == view.attempt_id);
            }
            out.push(view);
        }
        Ok(out)
    }

    /// Write the payer's activity record of a payment whose outcome waits on it again, and
    /// settle the outcome with it ([`PayOutcome::RecordHeld`], Paykit spec 7.2 step 3): the
    /// user has seen that the record holds a value that doesn't read, and has it replaced.
    /// The record is rebuilt from the journal, the request kept with the entry or a payment
    /// without a request's own members, with the acceptance and the proof, and written over
    /// the version the app holds, in the batch that writes `done` and queues the proof. A
    /// record that changed since is read again and refused with
    /// [`PaykitError::Conflict`]: the user looks again. A record that reads by then is
    /// updated as at any outcome, not replaced.
    ///
    /// Replacing it drops whatever the unreadable value held: a reader that saw a role the
    /// rebuilt record lacks ignores and flags the write (Paykit wire 5.3, "Seen roles").
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for an attempt whose outcome doesn't wait on its record;
    /// [`PaykitError::Conflict`] as above; [`Refusal::NotNow`] naming the scope keys while
    /// the record's value is sealed under a key the app doesn't hold yet, which opens once
    /// it comes and is never replaced meanwhile; the client's errors.
    pub async fn repair_activity_record(&mut self, attempt_id: &AttemptId) -> Result<PayOutcome> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.repair_activity_record_inner(attempt_id)).await
    }

    async fn repair_activity_record_inner(&mut self, attempt_id: &AttemptId) -> Result<PayOutcome> {
        self.refresh_own()?;
        // A repair writes over a value the app can't read, on the user's word: it waits for
        // the records to be listed, a wider certificate's keys included.
        self.records_known().await?;
        self.require_pay()?;
        let mut entry = self
            .journal()
            .await?
            .into_iter()
            .find(|e| e.attempt_id == attempt_id.to_text())
            .filter(|e| e.record_held && e.stage == Stage::RailPaid)
            .ok_or(Refusal::NotFound)?;
        match self.write_outcome(&mut entry, true, true).await? {
            Written::Done => {}
            Written::Held => {
                return Ok(PayOutcome::RecordHeld {
                    attempt_id: *attempt_id,
                })
            }
            Written::Divergent(record) => {
                return Ok(PayOutcome::RecordDivergent {
                    attempt_id: *attempt_id,
                    record,
                })
            }
        }
        if let Some(msg_id) = entry.proof_msg_id.clone() {
            let _ = self.app.flush_message(&msg_id).await;
        }
        Ok(PayOutcome::Paid {
            attempt_id: *attempt_id,
            proof_msg_id: entry.proof_msg_id.clone(),
        })
    }

    /// The journal, every entry read back bound to its key (`state::Stored`).
    pub(crate) async fn journal(&self) -> Result<Vec<JournalEntry>> {
        Ok(self
            .local_scan::<JournalEntry>(key::JOURNAL)
            .await?
            .into_iter()
            .map(|(_, entry)| entry)
            .collect())
    }

    /// The journal entry of `attempt`, read back checked and bound to its key: of that
    /// attempt, its claim key the one its payee and request blind to, its record the one its
    /// request or attempt names (`state::Stored`).
    ///
    /// # Errors
    ///
    /// A store failure, or an entry that doesn't read or isn't the attempt its key names.
    pub(crate) async fn journal_entry(&self, attempt: &AttemptId) -> Result<Option<JournalEntry>> {
        self.local_get::<JournalEntry>(&key::journal(attempt)).await
    }

    /// [`Paykit::journal_entry`], bound to `obligation` as well, the claim the caller looks
    /// the attempt up for ([`JournalEntry::on_claim`]).
    ///
    /// # Errors
    ///
    /// As [`Paykit::journal_entry`]; a corrupt store for an entry of the attempt on another
    /// claim.
    pub(crate) async fn journal_entry_on(
        &self,
        attempt: &AttemptId,
        obligation: &[u8; 32],
    ) -> Result<Option<JournalEntry>> {
        JournalEntry::on_claim(self.journal_entry(attempt).await?, obligation)
    }

    /// Journal `entry`, checked first as [`journal_write`] does: only a state a resume and a
    /// restore read back is persisted.
    async fn save(&mut self, entry: &JournalEntry) -> Result<()> {
        entry.check()?;
        self.local_put(&key::journal(&entry.attempt()?), entry)
            .await
    }

    pub(crate) async fn set_stage(&mut self, entry: &mut JournalEntry, stage: Stage) -> Result<()> {
        entry.stage = stage;
        self.save(entry).await
    }

    /// The attempts a restore left unresolved for the obligation, which make it "may be
    /// paid" (Paykit spec 7.2); an open entry of this app's otherwise is a payment in
    /// progress.
    async fn restored_attempts(&self, obligation: &[u8; 32]) -> Result<Vec<AttemptId>> {
        let mut restored = Vec::new();
        for entry in self.journal().await? {
            if &entry.obligation != obligation || !entry.open() {
                continue;
            }
            if !entry.restored {
                return Err(Refusal::PaymentInProgress.into());
            }
            restored.push(entry.attempt()?);
        }
        restored.sort();
        Ok(restored)
    }

    /// The step to attempt at, after the claim's checks (Paykit spec 7.1, 7.2): a SETTLED
    /// claim is paid; another open attempt is another wallet's; a pinned deadline other
    /// than this app's may already be paid, with no confirmation passing it. Otherwise the
    /// doubt, `restored` with the claim's, read now: a doubtful one is refused with it
    /// ([`PaymentDoubt`]) unless the user confirmed exactly it.
    async fn claim_step(
        &mut self,
        obligation: &[u8; 32],
        deadline: Option<u64>,
        restored: Vec<AttemptId>,
        options: &PayOptions,
    ) -> Result<u64> {
        let view = self.app.read_claim(obligation).await?.map(|read| read.view);
        if let Some(view) = &view {
            self.refuse_claim(view)?;
            if !view.attempts.is_empty() && view.deadline != deadline {
                return Err(Refusal::MayBePaid {
                    reason: "Paykit spec 7.1: the claim's pinned deadline isn't this app's".into(),
                    doubt: None,
                }
                .into());
            }
        }
        let kept = self.app.claim_doubt(obligation).await?.unwrap_or_default();
        let ids = |ids: Vec<[u8; 16]>| {
            let mut ids: Vec<AttemptId> = ids.into_iter().map(AttemptId::from_bytes).collect();
            ids.sort();
            ids.dedup();
            ids
        };
        let mut unknown = kept.unknown;
        if let Some(view) = &view {
            unknown.extend(
                view.attempts
                    .iter()
                    .filter(|a| a.outcome == Outcome::Unknown)
                    .map(|a| a.attempt_id),
            );
        }
        let doubt = PaymentDoubt {
            obligation: *obligation,
            step: view.as_ref().map(|v| v.step),
            restored,
            unknown: ids(unknown),
            orphaned: ids(kept.orphaned),
            next_possibly_duplicate: kept.next_possibly_duplicate
                || view.as_ref().is_some_and(|v| v.next_possibly_duplicate),
            divergent: kept.divergent,
            after_distrust: kept.after_distrust,
            early_pin: kept.early_pin,
        };
        if doubt.doubtful() && options.may_be_paid_confirmed.as_ref() != Some(&doubt) {
            let reason = if doubt.restored.is_empty() {
                "Paykit spec 7.1: a doubtful claim"
            } else {
                "Switch spec 12.4: a restored attempt is unresolved"
            };
            return Err(Refusal::MayBePaid {
                reason: reason.into(),
                doubt: Some(Box::new(doubt)),
            }
            .into());
        }
        Ok(view.map_or(0, |v| v.step))
    }

    fn refuse_claim(&self, view: &ClaimView) -> Result<()> {
        match view.status {
            ClaimStatus::Settled => Err(Refusal::AlreadyPaid.into()),
            ClaimStatus::Attempting if view.owner == Some(self.app_id()) => {
                Err(Refusal::PaymentInProgress.into())
            }
            ClaimStatus::Attempting => Err(Refusal::BeingHandledByOtherWallet.into()),
            ClaimStatus::Free => Ok(()),
        }
    }

    /// A destination from the payee's Private Payment Lists (Paykit spec 8.4): of the lists
    /// in use, of the app asked for or of every current app, the endpoints the payment may
    /// use, not a single-use endpoint already used at its list's version and, for a request
    /// that restricts them, one of its accepted identifiers, `allowed` (Paykit wire 4.5),
    /// are handed to the payment method, which chooses ([`Paykit::choose`]). In
    /// [`PaymentMode::Public`], the endpoints of the payee's public endpoints instead, from
    /// its current apps' records ([`crate::app_records`]), with the same restrictions. With
    /// no usable list, missing, expired or used up, the payee is asked for its current one
    /// ([`Paykit::ask_for_lists`]) and the refusal is [`Refusal::WaitingForList`]; a list in
    /// use that offers no endpoint the payment may use is [`Refusal::NoDestination`]; lists
    /// in use whose endpoints the method pays none of, and no expired list, are
    /// [`Refusal::UnsupportedEndpoint`], without asking.
    async fn list_destination(
        &mut self,
        payee: &[u8; 32],
        app: Option<[u8; 16]>,
        allowed: Option<&[PaymentEndpointIdentifier]>,
        amount: &PaymentAmount,
        options: &PayOptions,
    ) -> Result<Chosen> {
        let wanted = |id: &PaymentEndpointIdentifier| {
            options.endpoint.as_ref().is_none_or(|want| want == id)
                && allowed.is_none_or(|ids| ids.contains(id))
        };
        if let PaymentMode::Public(_) = options.mode {
            let mut records = self
                .public_records(
                    payee,
                    app,
                    Initiator::User,
                    options.unconfirmed_identity_confirmed,
                )
                .await??;
            // The payee's public Payment List is every current app's public endpoints together
            // (Paykit spec 4), handed to the method by App ID, then by identifier, so every
            // payer device hands the same for the same records.
            records.sort_by_key(|usable| usable.record.body().app_id);
            let gathered: Vec<Choice> = records
                .iter()
                .flat_map(|usable| {
                    usable
                        .member
                        .public_endpoints
                        .iter()
                        .filter(|(id, _)| wanted(id))
                        .map(|(id, payload)| {
                            let carrier = Carrier::Record(Box::new(usable.record.clone()));
                            Choice::new(id, payload, usable.destination(id, payload), carrier, None)
                        })
                })
                .collect();
            if gathered.is_empty() {
                return Err(Refusal::NoDestination(
                    "Paykit spec 4: the payee has no usable public endpoint".into(),
                )
                .into());
            }
            return self
                .choose(payee, gathered, amount, options)?
                .ok_or_else(|| unsupported("the payee's public endpoints"));
        }
        let mut lists = self.lists_of(payee).await?;
        lists.retain(|l| app.is_none_or(|a| a == l.app_id));
        // A list whose record is divergent, or was seen so, may be stale: the upload may hold
        // a newer one, withdrawing or replacing a destination. None of its endpoints,
        // single-use or reusable, is paid until the user confirms it (Paykit spec 6); with no
        // other list to pay, the payment is refused naming those records.
        let mut paused_lists = Vec::new();
        let mut usable = Vec::with_capacity(lists.len());
        for list in lists {
            let record = self
                .app
                .record_key(&list_target(&list.payee, &list.app_id))?;
            if self.pause_at(&record).await?.blocks_by_hand() {
                paused_lists.push(record);
            } else {
                usable.push(list);
            }
        }
        let mut lists = usable;
        // Only a current app's list counts (Paykit spec 8.4, 7.2): a removed app's list, of
        // whatever version it chose, would shadow the current apps' and then be refused by
        // the destination check. The payee's `identity.json` is the copy that check takes,
        // so a fresh one is read once here.
        if !lists.is_empty() {
            let identity = match self
                .payee_identity(
                    payee,
                    Need::FreshCopy,
                    Initiator::User,
                    options.unconfirmed_identity_confirmed,
                )
                .await?
            {
                Ok(identity) => identity,
                Err(refusal) => return Err(refusal.into()),
            };
            let now = self.now();
            lists.retain(|l| {
                let carrier = Carrier::Message(Box::new(l.list.clone()));
                crate::destination::current(&identity.doc, &l.app_id, &carrier, now).is_ok()
            });
        }
        // Taken after the read of the payee's identity.json: a list that expired while it
        // was read is expired here, and the payee asked for its current one, rather than
        // chosen and then refused by the destination check.
        let now = self.now();
        // Newest list first, then by App ID, so every payer device hands the method the same
        // for the same lists.
        lists.sort_by_key(|l| {
            let version = match l.list.body() {
                Body::PrivatePaymentList(list) => list.version,
                _ => 0,
            };
            (std::cmp::Reverse(version), l.app_id)
        });
        // The version held of each app's list, newest first, for a request that names it.
        let mut held: Vec<([u8; 16], u64)> = Vec::new();
        // Whether a list still in use offers an endpoint the payment would use, though each
        // one is used: a list used up, not one without such an endpoint.
        let mut offered = false;
        let mut live = false;
        // Whether some app's list has expired, which its current one may replace.
        let mut expired = false;
        // The endpoints of the lists in use that the payment may use, in the order above and
        // then by identifier; the single-use ones already used at their list's version are
        // left out (Paykit spec 8.4), and kept apart only to tell a list used up.
        let mut gathered = Vec::new();
        let mut used = Vec::new();
        for record in lists {
            let Body::PrivatePaymentList(list) = record.list.body() else {
                continue;
            };
            if !held.iter().any(|(app, _)| *app == record.app_id) {
                held.push((record.app_id, list.version));
            }
            let accepted = record.list.accepted_at().unwrap_or(0);
            if now >= list.usable_until(accepted) {
                expired = true;
                continue;
            }
            live = true;
            for (id, endpoint) in &list.payment_endpoints {
                if !wanted(id) {
                    continue;
                }
                offered = true;
                let destination = Destination::new(
                    record.app_id,
                    id,
                    &endpoint.payload,
                    DestinationSource::List,
                    Some(list.version),
                    endpoint.single_use,
                );
                let carrier = Carrier::Message(Box::new(record.list.clone()));
                // The version marked used is the one of the list whose endpoint is chosen.
                let reserve = endpoint.single_use.then(|| record.clone());
                let choice = Choice::new(id, &endpoint.payload, destination, carrier, reserve);
                let spent = record
                    .used
                    .iter()
                    .any(|u| u.version == list.version && &u.endpoint == id);
                if endpoint.single_use && spent {
                    used.push(choice);
                } else {
                    gathered.push(choice);
                }
            }
        }
        if let Some(chosen) = self.choose(payee, gathered, amount, options)? {
            return Ok(chosen);
        }
        if !paused_lists.is_empty() {
            self.refuse_while_paused(&paused_lists).await?;
        }
        // Lists in use that offer only endpoints the method doesn't pay, and no app's list to
        // ask again: a newer list needn't differ, so the payee isn't asked. A single-use
        // endpoint used up that the method pays is one a newer list may offer again.
        if offered && !expired && !self.pays_any(&used, amount)? {
            return Err(unsupported("the payee's Private Payment List endpoints"));
        }
        // A list still in use with no endpoint the payment may use isn't missing, expired or
        // used up: a newer one needn't differ, so the payee isn't asked.
        if live && !offered {
            return Err(Refusal::NoDestination(
                "Paykit spec 8.4: the payee's Private Payment List has no endpoint this payment \
                 may use"
                    .into(),
            )
            .into());
        }
        self.ask_for_lists(payee, app, &held).await?;
        Err(Refusal::WaitingForList.into())
    }

    /// The payment method's choice among `gathered` ([`crate::Rail::select_endpoints`]): each one
    /// handed in as a candidate whose ID is its place, in the order gathered, with `amount`.
    /// Returns the places of the ones it pays, in its order, and the first error of one it
    /// couldn't judge, by the order handed in.
    ///
    /// # Errors
    ///
    /// The method's error for the whole call; [`RailError::InvalidSelection`] for an ID that
    /// isn't one of the candidates', or one returned twice, or both as payable and in error:
    /// the method answered about something it wasn't asked.
    fn rail_order(&self, gathered: &[Choice], amount: Option<&PaymentAmount>) -> Result<Ordered> {
        let mut candidates = Vec::with_capacity(gathered.len());
        for (at, choice) in gathered.iter().enumerate() {
            let id = u32::try_from(at).map_err(|_| {
                Refusal::NoDestination("more endpoints than a payment method is handed".into())
            })?;
            candidates.push(PaymentEndpointCandidate::new(
                CandidateId::new(id),
                choice.endpoint_id.clone(),
                choice.payload.clone(),
            ));
        }
        let selection = self.rail.select_endpoints(&candidates, amount)?;
        let place = |id: CandidateId| {
            usize::try_from(id.get())
                .ok()
                .filter(|at| *at < candidates.len())
                .ok_or_else(|| {
                    RailError::InvalidSelection("a candidate ID it wasn't handed".into())
                })
        };
        let mut payable: Vec<usize> = Vec::new();
        for id in selection.payable {
            let at = place(id)?;
            if payable.contains(&at) {
                return Err(RailError::InvalidSelection("a candidate ID twice".into()).into());
            }
            payable.push(at);
        }
        let mut errors: Vec<(usize, RailError)> = Vec::new();
        for failed in selection.errors {
            let at = place(failed.id)?;
            if payable.contains(&at) || errors.iter().any(|(e, _)| *e == at) {
                return Err(RailError::InvalidSelection("a candidate ID twice".into()).into());
            }
            errors.push((at, failed.error));
        }
        let error = errors
            .into_iter()
            .min_by_key(|(at, _)| *at)
            .map(|(_, error)| error);
        Ok(Ordered { payable, error })
    }

    /// Whether the payment method pays any of `used`, the single-use endpoints left out as
    /// used, which a newer list may offer again. None is paid; a candidate the method
    /// couldn't judge fails the payment with its error, so the payee isn't taken to offer
    /// nothing it pays.
    fn pays_any(&self, used: &[Choice], amount: &PaymentAmount) -> Result<bool> {
        if used.is_empty() {
            return Ok(false);
        }
        let ordered = self.rail_order(used, Some(amount))?;
        if !ordered.payable.is_empty() {
            return Ok(true);
        }
        ordered.error.map_or(Ok(false), |error| Err(error.into()))
    }

    /// The endpoint a payment takes of `gathered` (Paykit spec 7.2): the first in the
    /// payment method's order ([`Paykit::rail_order`]) whose transfer check passes
    /// ([`Paykit::check_transfer`]), so a malformed or amount-incompatible payload falls to
    /// the next. With none taken: the error of a candidate the method couldn't judge, which
    /// may be payable once it can, so nothing is concluded from the others; else the first
    /// transfer refusal; else `None`, as when there are none or the method pays none of
    /// them. The choice is journaled with the attempt, and a resume pays it.
    fn choose(
        &self,
        payee: &[u8; 32],
        gathered: Vec<Choice>,
        amount: &PaymentAmount,
        options: &PayOptions,
    ) -> Result<Option<Chosen>> {
        if gathered.is_empty() {
            return Ok(None);
        }
        let ordered = self.rail_order(&gathered, Some(amount))?;
        let mut gathered: Vec<Option<Choice>> = gathered.into_iter().map(Some).collect();
        let mut refused = None;
        for at in ordered.payable {
            let Some(choice) = gathered.get_mut(at).and_then(Option::take) else {
                continue;
            };
            match self.check_transfer(payee, &choice.destination, amount, options) {
                Ok(bound) => {
                    return Ok(Some((
                        choice.destination,
                        choice.carrier,
                        choice.reserve,
                        bound,
                    )))
                }
                Err(error) if matches!(error.refusal(), Some(Refusal::Transfer { .. })) => {
                    refused.get_or_insert(error);
                }
                Err(error) => return Err(error),
            }
        }
        if let Some(error) = ordered.error {
            return Err(error.into());
        }
        refused.map_or(Ok(None), Err)
    }

    /// The refusal of a payment of the request whose record is at `target`,
    /// once its deadline has passed (Paykit spec 7.1), decided on the record the switch holds,
    /// read again, never on a copy held: another own app may have rejected or cancelled the
    /// request, or the payee cancelled it, as the request's status shows before its expiry.
    /// [`Refusal::DeadlinePassed`] only for a record that is there, reads, is bound to this
    /// request's key, holds this request, and leaves it open, as the switch's current write
    /// ([`Paykit::fresh_request`]); a closed one gives what closed it. The switch holding none
    /// (`404`), or a newer write this app refused, is no evidence the request is open: it
    /// gives `otherwise`, as any other record does: the error the attempt met, or, without one,
    /// [`Refusal::NotFound`] for none and [`PaykitError::Conflict`] for one that doesn't read
    /// as this request. The caller has taken the claim's state first.
    ///
    /// # Errors
    ///
    /// The read's failure: a record that couldn't be read again is never taken as unchanged.
    async fn past_deadline(
        &mut self,
        target: &ActivityTarget,
        otherwise: Option<PaykitError>,
    ) -> Result<PaykitError> {
        Ok(match self.fresh_request(target).await? {
            Fresh::Held(fresh) if fresh.shows_paid() => Refusal::AlreadyPaid.into(),
            Fresh::Held(fresh) if rejected(&fresh) => Refusal::Rejected.into(),
            Fresh::Held(fresh) if cancelled(&fresh) => Refusal::Cancelled.into(),
            Fresh::Held(_) => Refusal::DeadlinePassed.into(),
            Fresh::Missing => otherwise.unwrap_or_else(|| Refusal::NotFound.into()),
            Fresh::Unreadable => otherwise.unwrap_or_else(unreadable_request),
        })
    }

    /// Paykit spec 8.4, "No usable list": ask the payee for its current list, with a
    /// `paykit.private_payment_list_request` naming the version held (Paykit wire 4.14). With
    /// `app`, of that app; without, of each app whose list is held, or one request naming no
    /// app when none is. A request already asked of the same app at the same held version is
    /// outstanding, and not asked again, until a day has passed: the payee can't be forced to
    /// answer, and a request it dropped is asked once more after that, so a payer waits at
    /// most a day for one more chance. The request is queued before its mark is kept: a crash
    /// between the two asks again, rather than never.
    async fn ask_for_lists(
        &mut self,
        payee: &[u8; 32],
        app: Option<[u8; 16]>,
        held: &[([u8; 16], u64)],
    ) -> Result<()> {
        let held_of = |app: [u8; 16]| held.iter().find(|(a, _)| *a == app).map(|(_, v)| *v);
        let asks: Vec<(Option<[u8; 16]>, Option<u64>)> = match app {
            Some(app) => vec![(Some(app), held_of(app))],
            None if held.is_empty() => vec![(None, None)],
            None => held.iter().map(|(app, v)| (Some(*app), Some(*v))).collect(),
        };
        let now = self.now();
        for (app_id, held_version) in asks {
            let mark = key::list_ask(payee, app_id.as_ref());
            // Read bound to its key: a mark naming another payee or app than its key is a
            // corrupt store (`state::Stored`).
            if let Some(asked) = self.local_get::<ListAsk>(&mark).await? {
                // A mark ahead of the clock (restored, or the clock set back) is due, as a
                // lists read is: it never holds the next request back for longer than a day.
                if asked.held_version == held_version
                    && asked.asked_at_ms <= now
                    && now - asked.asked_at_ms < DAY_MS
                {
                    continue;
                }
            }
            let body = PrivatePaymentListRequest {
                held_version,
                app_id,
            }
            .to_body()?;
            let signed = self.sign(*payee, Kind::PrivatePaymentListRequest, Vec::new(), body)?;
            let msg_id = self.app.queue_signed(signed).await?;
            self.local_put(
                &mark,
                &ListAsk {
                    payee: *payee,
                    app_id,
                    held_version,
                    asked_at_ms: now,
                },
            )
            .await?;
            // Queued, the request goes at the next flush if this one fails: the refusal still
            // tells the user the app waits for the payee.
            let _ = self.app.flush_message(&msg_id).await;
        }
        Ok(())
    }

    /// The transfer check (Paykit spec 7.2 step 1): the method transfers exactly the amount
    /// paid, and any fee the payee can influence is within the app's cap. A transfer that
    /// passes is what the user asked for when they called pay, and is paid with no other
    /// consent. A destination failing it is paid only on the user's explicit choice of
    /// exactly what it would transfer: the method is asked again, and the destination and
    /// its transfer must be the ones the user confirmed ([`TransferQuote`]); otherwise it is
    /// refused with what it would transfer now, for the user to see. Returns the bound the
    /// attempt is journaled with, which the final payment check holds the method to again.
    fn check_transfer(
        &self,
        payee: &[u8; 32],
        destination: &Destination,
        amount: &PaymentAmount,
        options: &PayOptions,
    ) -> Result<TransferBound> {
        let endpoint_id = destination.endpoint_id()?;
        let payload = destination.payload()?;
        let transfer = self.rail.transfer(&endpoint_id, &payload, amount);
        let cap = self.config.fee_caps.get(&amount.asset);
        let kept_cap = cap.map(|cap| cap.as_str().to_owned());
        let Some(reason) = transfer_refused(&transfer, amount, cap) else {
            return Ok(TransferBound::Asked { cap: kept_cap });
        };
        let quote = TransferQuote {
            payee: *payee,
            app_id: destination.app_id,
            endpoint_id,
            payload_hash: pubky_switch_core::enc::hash(payload.as_str().as_bytes()),
            amount: amount.clone(),
            transfer,
        };
        if options.transfer_confirmed.as_ref() == Some(&quote) {
            return Ok(TransferBound::Confirmed {
                cap: kept_cap,
                amount: quote.transfer.amount.as_ref().map(KeptAmount::of),
                max_fee: quote
                    .transfer
                    .max_fee
                    .as_ref()
                    .map(|f| f.as_str().to_owned()),
            });
        }
        Err(Refusal::Transfer {
            reason: reason.into(),
            quote: Box::new(quote),
        }
        .into())
    }

    /// The transfer check again, at the final payment check just before the call, a
    /// repeat's included (Paykit spec 7.2 step 1): the method's transfer now must still be
    /// within the bound the attempt was journaled with, so a fee estimate that grew, or an
    /// amount that changed, since the attempt was planned is never paid. A transfer that is
    /// what the user asked for, within the cap at planning, passes; a confirmed one passes
    /// also when it is the one confirmed. `Err` inside holds the refusal, with what the
    /// method would transfer now.
    fn transfer_holds(&self, entry: &JournalEntry) -> Result<std::result::Result<(), Refusal>> {
        let endpoint_id = entry.destination.endpoint_id()?;
        let payload = entry.destination.payload()?;
        let amount = entry.amount.amount()?;
        let transfer = self.rail.transfer(&endpoint_id, &payload, &amount);
        let parse = |cap: &Option<String>| {
            cap.as_deref()
                .map(paykit_lib::Decimal::parse)
                .transpose()
                .map_err(|_| PaykitError::corrupt())
        };
        let (cap, confirmed) = match &entry.transfer_bound {
            Some(TransferBound::Asked { cap }) => (parse(cap)?, None),
            Some(TransferBound::Confirmed {
                cap,
                amount,
                max_fee,
            }) => (parse(cap)?, Some((amount, max_fee))),
            None => (self.config.fee_caps.get(&amount.asset).cloned(), None),
        };
        let reason = transfer_refused(&transfer, &amount, cap.as_ref());
        let as_confirmed = confirmed.is_some_and(|(kept_amount, max_fee)| {
            transfer.amount.as_ref().map(KeptAmount::of) == *kept_amount
                && transfer.max_fee.as_ref().map(|f| f.as_str().to_owned()) == *max_fee
        });
        let Some(reason) = reason.filter(|_| !as_confirmed) else {
            return Ok(Ok(()));
        };
        Ok(Err(Refusal::Transfer {
            reason: format!("{reason}, since the attempt was planned"),
            quote: Box::new(TransferQuote {
                payee: entry.payee,
                app_id: entry.destination.app_id,
                endpoint_id,
                payload_hash: pubky_switch_core::enc::hash(payload.as_str().as_bytes()),
                amount,
                transfer,
            }),
        }))
    }

    /// Steps 1 to 4 of the paying order for a planned attempt (Paykit spec 7.2).
    async fn execute(&mut self, plan: Plan) -> Result<PayOutcome> {
        let Plan {
            mut entry,
            record,
            carrier,
            step,
            reserve,
            create,
            on_record,
            by,
            unconfirmed,
            end_hold,
        } = plan;
        let attempt_id = entry.attempt()?;
        // The attempt's `orphan_after` covers the rail's longest time and the pre-dispatch
        // budget, as the rail declares it now (Switch spec 10.2).
        crate::paykit::check_orphan_window(&self.config, self.rail.as_ref())?;
        // The carrier is journaled with the entry, for the checks before any repeat.
        entry.keep_carrier(&carrier);
        // The own identity.json, read again and confirmed, before any write: no attempt
        // without a copy revalidated in the last 60 s (Paykit spec 4, Switch spec 12.6).
        self.own_identity_confirmed(true).await?;
        // The payee's identity.json, before any write: a payment the user started whose
        // payee's switch couldn't confirm it needs the user's confirmation first, and an
        // automatic one isn't made (Paykit spec 7.2 step 1, Switch spec 4.3).
        let request_hash = entry.request_hash()?;
        if let Err(refusal) = self
            .check_destination(
                &entry.payee,
                &entry.destination,
                &carrier,
                request_hash.as_ref(),
                by,
                unconfirmed,
            )
            .await?
        {
            return Err(refusal.into());
        }
        let op = ClaimOp::attempt(
            entry.obligation,
            step,
            attempt_id.to_bytes(),
            self.config.orphan_after_s_for(&self.rail.declaration()),
            entry.deadline,
        );
        let mut conditions = Vec::new();
        if on_record {
            let key = self.activity_key(&record)?;
            conditions.push(self.app.record_condition(&key).await?);
        }
        let mut batch = OpBatch::new()
            .op_if(op.kind(), conditions)
            .obligation_deadline(entry.obligation, own_deadline(entry.deadline));
        if let Some(record_key) = end_hold {
            // The user saw the payee may have cancelled it (Paykit spec 7.2): the hold ends
            // with the attempt, and stays when the attempt isn't made.
            batch = batch.local(state::local_put(&key::hold_confirmed(&record_key), &true)?);
        }
        if let Some(mut list) = reserve {
            // A single-use endpoint is marked used in the attempt's batch (Paykit spec 7.2
            // step 1, 8.4).
            list.used.push(UsedEndpoint {
                version: entry
                    .destination
                    .list_version
                    .ok_or_else(PaykitError::corrupt)?,
                endpoint: entry.destination.endpoint_id()?,
                attempt_id,
                origin: paykit_lib::records::Origin::default(),
            });
            let value = list.to_json()?;
            batch = batch.entry(
                self.app
                    .record_op(
                        &list_target(&list.payee, &list.app_id),
                        Some(&value),
                        Vec::new(),
                    )
                    .await?,
            );
        }
        if let Some(created) = &create {
            // A payment without a request: its activity record (Paykit spec 8.4).
            let value = created.to_json()?;
            batch = batch.entry(
                self.app
                    .record_op(&record.record_target(), Some(&value), Vec::new())
                    .await?,
            );
        }
        // The journal moves to `Attempted` with the batch's answer, never before.
        let mut attempted = entry.clone();
        attempted.stage = Stage::Attempted;
        batch = batch.local(journal_write(&attempted)?);
        // The attempt ID is journaled before the write (Paykit spec 7.2), once the batch is
        // built: a batch that can't be built leaves no open entry behind it.
        self.save(&entry).await?;
        // The attempt's age before its dispatch (the pre-dispatch budget,
        // [`crate::PRE_DISPATCH_BUDGET_MS`]) counts from before it is sent, on both device
        // clocks: the request, the answer and the answer's local commit are all in it, so
        // the age is never less than the time since the service accepted the attempt.
        let (answered_wall_ms, answered_at) = (self.now(), std::time::Instant::now());
        // The client checks the attempt's proof: only one that shows this app's own open
        // attempt lets money move (Paykit spec 7.2; Switch wire 6.5 "Binding").
        let submitted = self.app.submit_batch(batch).await;
        let submitted = match submitted {
            Ok(submitted) => submitted,
            Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                self.set_stage(&mut entry, Stage::Abandoned).await?;
                // What the answer carries decides first: the claim it shows, with the open
                // attempt of another wallet, or the payment made (Paykit spec 7.1). Then the
                // request record the switch holds, read again, which another own app may
                // have rejected or cancelled meanwhile, or the payee cancelled; a read that
                // fails is the error, never taken as the record unchanged. Only then the
                // deadline, at the answer's service time.
                if let Some(view) = claim_conflict(error.unkept()) {
                    self.refuse_claim(&view)?;
                }
                let changed = PaykitError::Conflict(
                    "Paykit spec 7.2: the request or list record changed meanwhile".into(),
                );
                if self.deadline_passed(entry.deadline) {
                    if on_record {
                        return Err(self.past_deadline(&record, Some(changed)).await?);
                    }
                    return Err(Refusal::DeadlinePassed.into());
                }
                return Err(changed);
            }
            // These two are matched bare: a refusal or a proof that showed another state left
            // nothing kept. The same wrapped in `Kept` keeps the batch, the attempt in it, so
            // it goes to the last arm, as a lost answer does.
            Err(AppError::InvalidData(why)) => {
                // The attempt's proof showed another state: the client wrote it `failed`.
                self.set_stage(&mut entry, Stage::Finished).await?;
                return Err(Refusal::AttemptFailed(Box::new(Refusal::Destination(why))).into());
            }
            Err(error @ AppError::Refused(_)) => {
                self.set_stage(&mut entry, Stage::Abandoned).await?;
                return Err(error.into());
            }
            // The answer may have been lost: the batch is kept and the journal says no money
            // moved, so a resume reads the claim and writes `failed` if the attempt is ours.
            // While the client still keeps the batch, the claim can't show the attempt, and a
            // resume leaves the entry as it is ([`Paykit::attempt_still_kept`]).
            Err(error) => return Err(error.into()),
        };
        let proof = submitted.proofs.first();
        entry.step = proof.and_then(|proof| proof.body().step);
        entry.attempt_proof = proof.map(|proof| proof.signed().clone());
        // The view the attempt's checked proof shows (Switch wire 6.5 "Binding").
        let claim = proof
            .map(|proof| ClaimView::from_value(&proof.body().state))
            .transpose()?;
        self.set_stage(&mut entry, Stage::Attempted).await?;
        // Before any money moves, the gate every dispatch passes (Paykit spec 4, 7.2 step 1).
        // The copies checked before the write are still fresh: this reads no network unless
        // they aged meanwhile (Switch spec 4.3). A failure writes `failed` instead of paying,
        // but for an attempt at a service the identity no longer names, which the move rules
        // resolve (Switch spec 12.5, 12.6): nothing is written there.
        let Some(claim) = claim else {
            return self
                .fail_attempt(&mut entry, Refusal::NotNow("no attempt proof".into()))
                .await;
        };
        let Some(opened_at_ms) = claim
            .attempt(&attempt_id.to_bytes())
            .map(|attempt| attempt.opened_at)
        else {
            return self
                .fail_attempt(
                    &mut entry,
                    Refusal::NotNow("no attempt in its proof".into()),
                )
                .await;
        };
        let answered = Answered {
            opened_at_ms,
            wall_ms: answered_wall_ms,
            at: answered_at,
        };
        let doc = match self
            .pay_gate(&entry, &carrier, &claim, by, unconfirmed, false)
            .await?
            .map_err(Held::refusal)
        {
            Ok(doc) => doc,
            Err(refusal @ Refusal::NotCurrent(_)) => return Err(refusal.into()),
            Err(refusal) => return self.fail_attempt(&mut entry, refusal).await,
        };
        // The entry stays `Attempted`, known unsubmitted, through the acceptance's journaling
        // and the final payment check: either can stop the payment, and a resume then writes
        // the attempt `failed` rather than taking it as possibly dispatched. `Paying` is
        // journaled just before the payment method's call ([`Paykit::dispatch`]). The
        // acceptance is signed and journaled now, and sendable only once that call is made.
        let acceptance = self.journal_acceptance(&mut entry).await?;
        self.dispatch(
            entry,
            doc,
            by,
            unconfirmed,
            Dispatch::First { answered },
            acceptance,
        )
        .await
    }

    /// The own `identity.json` revalidated in the last [`REVALIDATION_MS`] (Paykit spec 4,
    /// "Fresh before paying"; Switch spec 12.6 "Residual risk"): with `refresh`, it is read
    /// again first, and without it only once its last confirmation is that old. A read that
    /// confirms the copy held, or a confirmation under 60 s old, lets the payment go on;
    /// `Missing`, `Older` and `Unavailable` confirm nothing, and an error refuses.
    pub(crate) async fn own_identity_confirmed(
        &mut self,
        refresh: bool,
    ) -> std::result::Result<(), Refusal> {
        if refresh || !self.own_identity_fresh() {
            // A Homeserver that can't be read now confirms nothing (`Unavailable`), and the
            // confirmation held still counts if it is fresh once the read is over. An error
            // is more: a copy that doesn't verify, a store that fails, or an alarm, such as
            // the own identity.json equivocating (Switch wire 3.3), and it stops the payment.
            if let Err(error) = self.app.refresh_identity().await {
                return Err(Refusal::IdentityUnavailable(format!(
                    "Paykit spec 4: the own identity.json can't be read again: {error}"
                )));
            }
        }
        // Freshness is judged on the clock after the read, never on a time read before it: a
        // slow read that fails can't stretch the 60 s (Paykit spec 4).
        if self.own_identity_fresh() {
            return Ok(());
        }
        Err(Refusal::IdentityUnavailable(
            "Paykit spec 4: the own identity.json wasn't confirmed in the last 60 s".into(),
        ))
    }

    /// Whether the own `identity.json` was confirmed less than [`REVALIDATION_MS`] before the
    /// device clock now. A confirmation the clock puts in the future is no evidence of a
    /// recent one: the clock went back.
    fn own_identity_fresh(&self) -> bool {
        let now = self.now();
        self.app
            .identity_confirmed_at()
            .and_then(|at| now.checked_sub(at))
            .is_some_and(|age| age < REVALIDATION_MS)
    }

    /// The latest service time the app has seen (Switch spec 10.1), for the deadline of a
    /// payment without a request (Paykit spec 8.4): the highest it verified when that is
    /// recent on the device clock, else the one a read of the payment's own claim, which a
    /// new attempt ID leaves empty, answers with. Deadlines run on the service's clock,
    /// never the device's: an old time would set a deadline that has passed already, and the
    /// device's could be off by the clock tolerance.
    ///
    /// # Errors
    ///
    /// The client's errors when the switch can't be read now: no attempt could be written
    /// then either.
    async fn recent_service_time(&mut self, obligation: &[u8; 32]) -> Result<u64> {
        let recent = |seen: Option<u64>, now: u64| {
            seen.filter(|t| {
                now.checked_sub(*t)
                    .is_none_or(|age| age < SERVICE_TIME_RECENT_MS)
            })
        };
        if let Some(seen) = recent(self.service_time(), self.now()) {
            return Ok(seen);
        }
        self.app.read_claim(obligation).await?;
        self.service_time().ok_or_else(|| {
            Refusal::NotNow("Switch spec 10.1: no service time verified".into()).into()
        })
    }

    /// Whether `deadline` is at or before the highest service time the app verified: no
    /// attempt is made then (Paykit spec 7.1).
    pub(crate) fn deadline_passed(&self, deadline: Option<u64>) -> bool {
        deadline.is_some_and(|d| self.service_time().is_some_and(|t| d <= t))
    }

    /// A check failed after the attempt and before any money moved: `failed` instead of
    /// paying (Paykit spec 7.2 step 1).
    async fn fail_attempt(&mut self, entry: &mut JournalEntry, why: Refusal) -> Result<PayOutcome> {
        self.write_outcome(entry, false, false).await?;
        Err(Refusal::AttemptFailed(Box::new(why)).into())
    }

    /// The acceptance owed with the payment, signed and journaled, not queued: nothing can
    /// send it yet (Paykit spec 7.2 step 2). `None` when none is owed or it went out already.
    async fn journal_acceptance(
        &mut self,
        entry: &mut JournalEntry,
    ) -> Result<Option<SignedMessage>> {
        if !entry.acceptance_owed || entry.acceptance_released {
            return Ok(None);
        }
        let Some(signed) = self.sign_acceptance(entry)? else {
            return Ok(None);
        };
        self.save(entry).await?;
        Ok(Some(signed))
    }

    /// Whether a restored entry's released acceptance was lost with the restore: the client's
    /// backup carries no outbox or sent messages, so a restored client that doesn't know its
    /// `msg_id` never sent it from this store, and it may never have left the old one. It is
    /// sent again only while the payer's record holds no acceptance: once placed, the record
    /// is what the payee's apps read. A non-restored entry's client always knows its own
    /// queued or sent message; an unreadable record leaves it for a later resume.
    async fn restored_acceptance_lost(&self, entry: &JournalEntry) -> Result<bool> {
        if !entry.restored {
            return Ok(false);
        }
        if let Some(msg_id) = &entry.acceptance_msg_id {
            if self.app.delivery(msg_id).await?.is_some() {
                return Ok(false);
            }
        }
        let target = ActivityTarget::payer(entry.natural_key.clone());
        let held = self.app.shared_record(&self.activity_key(&target)?).await?;
        Ok(match held.as_ref().map(|r| &r.content) {
            Some(RecordContent::Value(value)) => {
                activity::read(Some(value), &self.identity(), ActivitySide::Payer)
                    .is_some_and(|record| record.response.is_none())
            }
            Some(RecordContent::Unreadable(_)) => false,
            None | Some(RecordContent::Deleted) => true,
        })
    }

    /// A new acceptance of `entry`'s request, set in the entry, not saved.
    fn sign_acceptance(&self, entry: &mut JournalEntry) -> Result<Option<SignedMessage>> {
        let Some(request_hash) = entry.request_hash()? else {
            return Ok(None);
        };
        // A new acceptance goes only under a certificate that may send one: after a renewal
        // that dropped `pay`, none is signed (Paykit spec 3.3); one signed before keeps its
        // own certificate ([`JournalEntry::acceptance_cert`]).
        self.require_pay()?;
        let label = entry.label()?;
        let signed = self.sign(
            entry.payee,
            Kind::PaymentRequestAcceptance,
            vec![request_hash],
            PaymentRequestAcceptance { label }.to_body()?,
        )?;
        entry.acceptance_msg_id = Some(signed.msg_id().to_owned());
        entry.acceptance_plaintext = Some(signed.plaintext().encode());
        entry.acceptance_cert = Some(self.app.cert().signed().clone());
        Ok(Some(signed))
    }

    /// Step 2 (Paykit spec 7.2): the acceptance goes into the outbox, sendable, and out
    /// while the payment runs. Called only once the payment method's call was made, or once
    /// a resume finds the payment in the method's history, never before: a payee never sees
    /// an acceptance of a payment the payer knows never ran. `signed` is the acceptance
    /// journaled for this dispatch; without it (a resume, a repeat) the journaled one is
    /// used if the outbox holds it already, and otherwise a new one is signed, the journaled
    /// one never having left this app.
    pub(crate) async fn release_acceptance(
        &mut self,
        entry: &mut JournalEntry,
        mut signed: Option<SignedMessage>,
    ) -> Result<()> {
        if !entry.acceptance_owed {
            return Ok(());
        }
        if entry.acceptance_released {
            if !self.restored_acceptance_lost(entry).await? {
                return Ok(());
            }
            // Lost with the restore: a new acceptance is signed, journaled in place of the
            // old one, queued, then marked, as a first release is below, so only it is ever
            // queued; a payee that got the old one before the backup keeps that one and notes
            // this `msg_id` in its record (Paykit spec 7.2 step 2).
            entry.acceptance_released = false;
            entry.acceptance_msg_id = None;
            entry.acceptance_plaintext = None;
            entry.acceptance_cert = None;
            signed = None;
        }
        let queued = match &entry.acceptance_msg_id {
            Some(msg_id) if signed.is_none() => self.app.delivery(msg_id).await?.is_some(),
            _ => false,
        };
        if !queued {
            let signed = match signed {
                Some(signed) => signed,
                None => match self.sign_acceptance(entry)? {
                    Some(signed) => signed,
                    None => return Ok(()),
                },
            };
            // Journaled, queued, then marked: a crash before the mark leaves it journaled and
            // queued, and the outbox shows it at the next release, so it is never signed
            // twice once it can be sent.
            self.save(entry).await?;
            self.app.queue_signed(signed).await?;
        }
        entry.acceptance_released = true;
        self.save(entry).await?;
        if let Some(msg_id) = entry.acceptance_msg_id.clone() {
            // Not awaited for paying: a delivery failure leaves it queued.
            let _ = self.app.flush_message(&msg_id).await;
        }
        Ok(())
    }

    /// An outcome written while its acceptance couldn't be queued ([`Stage::AcceptanceOwed`]):
    /// the acceptance is queued now, placed in the payer's activity record, and the entry
    /// finished in the batch of that update. Nothing else is written again: `done` and the
    /// proof went with the outcome. A record whose value doesn't read keeps the entry owed,
    /// as one that changed meanwhile does, for the next resume; one that is gone has nothing
    /// to place the acceptance in, and the entry finishes.
    ///
    /// # Errors
    ///
    /// The acceptance's queueing, or the store: the entry stays owed.
    pub(crate) async fn finish_owed_acceptance(&mut self, entry: &mut JournalEntry) -> Result<()> {
        let target = ActivityTarget::payer(entry.natural_key.clone());
        let own = self.identity();
        let key = self.activity_key(&target)?;
        // An acceptance a resume sends is an automatic one: none goes, nor is placed, while
        // the request record is paused or its view divergent (Paykit spec 6); the entry
        // stays owed, for a resume after the user's confirmation and a clear view.
        if self.pause_at(&key).await?.blocks_automatic() {
            return Ok(());
        }
        self.release_acceptance(entry, None).await?;
        let held = self.app.shared_record(&key).await?;
        let write = match held.as_ref().map(|r| &r.content) {
            Some(RecordContent::Value(value)) => {
                let Some(mut record) = activity::read(Some(value), &own, ActivitySide::Payer)
                else {
                    return Ok(());
                };
                if self.place_acceptance(&mut record, entry)? {
                    let value = record.to_json()?;
                    Some(
                        self.app
                            .record_op(&target.record_target(), Some(&value), Vec::new())
                            .await?,
                    )
                } else {
                    None
                }
            }
            Some(RecordContent::Unreadable(_)) => return Ok(()),
            None | Some(RecordContent::Deleted) => None,
        };
        let mut finished = entry.clone();
        finished.stage = Stage::Finished;
        match write {
            Some(write) => {
                let batch = OpBatch::new().entry(write).local(journal_write(&finished)?);
                match self.app.submit_batch(batch).await {
                    Ok(_) => {}
                    Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                        self.app.read_record(&target.record_target()).await?;
                        return Ok(());
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            None => self.save(&finished).await?,
        }
        *entry = finished;
        Ok(())
    }

    /// Step 2: pay through the rail with the attempt ID as the idempotency key.
    async fn dispatch(
        &mut self,
        mut entry: JournalEntry,
        payee: PeerIdentity,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        kind: Dispatch,
        acceptance: Option<SignedMessage>,
    ) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        let payment = RailPayment {
            endpoint_id: entry.destination.endpoint_id()?,
            payload: entry.destination.payload()?,
            amount: entry.amount.amount()?,
            idempotency_key: attempt_id,
            payment_reference: entry.payment_reference.clone(),
        };
        // Nothing is submitted unless the final payment check passes. Held, the attempt
        // stays open for a resume to decide: a first dispatch is still `Attempted` then,
        // known unsubmitted, and the resume writes it `failed`.
        let (check, payee) = self
            .final_payment_check(&entry, payee, by, confirmed, kind)
            .await?;
        match check {
            FinalCheck::Pay => {}
            FinalCheck::Hold => return Ok(PayOutcome::Pending { attempt_id }),
            // The method may have forgotten the key: the repeat isn't made, and the attempt
            // stays "may be paid" (Paykit spec 7.2).
            FinalCheck::Lapsed => return Ok(PayOutcome::Uncertain { attempt_id }),
            // A refused answer stops the payment. A first dispatch submitted nothing, so it
            // is written `failed` and refused with the answer's refusal; a repeat may follow
            // a dispatch that paid, so it stays "may be paid", for a receipt or the user.
            FinalCheck::Refused(refusal) => {
                return match kind {
                    Dispatch::First { .. } => self.fail_attempt(&mut entry, refusal).await,
                    Dispatch::Repeat { .. } => Ok(PayOutcome::Uncertain { attempt_id }),
                }
            }
        }
        // Starting to pay is journaled just before the payment begins (Paykit spec 7.2): from
        // here on it may be paid. A repeat's entry is `Paying` already, and nothing awaits
        // between its final check and its call.
        if matches!(kind, Dispatch::First { .. }) {
            self.set_stage(&mut entry, Stage::Paying).await?;
            // The commit was an await: the fence judges every gate again, and nothing awaits
            // after it before the call.
            match self.payment_fence(&entry, &payee, by, confirmed, kind)? {
                FinalCheck::Pay => {}
                stopped => return self.stopped_at_the_fence(entry, stopped).await,
            }
        }
        let outcome = self.rail.pay(payment).await;
        // The call was made and the payment may be running, its outcome not known yet: only
        // now may the payee hear it was accepted (Paykit spec 7.2 step 2). A payment the
        // method reports made is journaled with its proof first, and its report releases the
        // acceptance, so nothing sent ever runs ahead of the proof; a refused or failed one
        // releases none. A failure to queue it decides nothing about paying: the outcome or
        // a resume releases it once the payment shows.
        let acceptance = if matches!(
            outcome,
            Ok(RailOutcome::Pending) | Err(RailError::Unavailable(_))
        ) {
            let _ = self.release_acceptance(&mut entry, acceptance).await;
            None
        } else {
            acceptance
        };
        match outcome {
            Ok(outcome) => self.take_outcome(&mut entry, outcome, acceptance).await,
            // A refusal is authoritative: nothing was and nothing will be submitted, so a
            // first dispatch is definitively not paid, written `failed`, and may be replaced
            // (Paykit spec 7.2). A repeat's refusal says nothing of the dispatch before it,
            // which may have paid: the attempt stays open.
            Err(RailError::Refused(why)) if matches!(kind, Dispatch::First { .. }) => {
                self.set_stage(&mut entry, Stage::RailFailed).await?;
                self.report(&mut entry).await.map(|outcome| match outcome {
                    PayOutcome::Failed { attempt_id, .. } => PayOutcome::Failed {
                        attempt_id,
                        reason: format!("the payment method refused the call: {why}"),
                    },
                    other => other,
                })
            }
            // Any other error, once a payment may have been submitted, is nothing yet: the
            // attempt stays open (Paykit spec 7.2).
            Err(_) => Ok(PayOutcome::Pending { attempt_id }),
        }
    }

    /// A first dispatch the fence stopped after `Paying` was journaled: nothing was
    /// submitted, so the entry is written back to `Attempted`, known unsubmitted, as a held
    /// final check leaves it. Then, with nothing paying, a lookup answer the fence saw come
    /// is taken: one the client refuses, such as an equivocation (Switch wire 3.3), writes
    /// the attempt `failed`, as a refused answer before `Paying` does. `Attempted` rather
    /// than `failed` at once: it is one local commit, the state a held check leaves, and a
    /// resume writes the open attempt `failed` from it ("no payment started").
    ///
    /// The write-back can be lost, to a crash or a store failure: the entry then stays
    /// `Paying`, which a restart reads as possibly dispatched, resolved by the method's
    /// history or else "may be paid". That is the safe direction: a payment not made can be
    /// taken for one that may have been, never the other way.
    async fn stopped_at_the_fence(
        &mut self,
        mut entry: JournalEntry,
        stopped: FinalCheck,
    ) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        self.set_stage(&mut entry, Stage::Attempted).await?;
        let refused = match stopped {
            FinalCheck::Refused(refusal) => Some(refusal),
            _ => self.take_kept_lookup(&entry.payee).await?.err(),
        };
        match refused {
            Some(refusal) => self.fail_attempt(&mut entry, refusal).await,
            None => Ok(PayOutcome::Pending { attempt_id }),
        }
    }

    /// The method's outcome of `entry`'s dispatch, journaled and reported. `acceptance` is
    /// the one this dispatch signed, released once a payment made is journaled.
    async fn take_outcome(
        &mut self,
        entry: &mut JournalEntry,
        outcome: RailOutcome,
        acceptance: Option<SignedMessage>,
    ) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        match outcome {
            RailOutcome::Paid(RailPaid {
                proof, paid_at_ms, ..
            }) => {
                // The proof is journaled first: nothing after it can lose the payment. Then
                // the acceptance goes, the one signed for this dispatch (Paykit spec 7.2
                // step 2); one that can't be queued now is left to the report.
                entry.rail_proof = Some(proof);
                entry.paid_at_ms = paid_at_ms;
                self.set_stage(entry, Stage::RailPaid).await?;
                let _ = self.release_acceptance(entry, acceptance).await;
            }
            RailOutcome::Failed => {
                entry.failed_by_method = true;
                self.set_stage(entry, Stage::RailFailed).await?;
            }
            RailOutcome::Pending => {
                // The method shows the payment running, so its acceptance may go (Paykit spec
                // 7.2 step 2); one the dispatch couldn't queue is queued now, best effort, at
                // every resume that finds it still pending.
                let _ = self.release_acceptance(entry, acceptance).await;
                return Ok(PayOutcome::Pending { attempt_id });
            }
        }
        self.report(entry).await
    }

    /// Steps 3 and 4: the outcome, the activity record and the proof, in one batch, then
    /// the proof is sent.
    pub(crate) async fn report(&mut self, entry: &mut JournalEntry) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        let paid = entry.rail_proof.is_some();
        match self.write_outcome(entry, paid, false).await {
            Ok(Written::Done) => {}
            Ok(Written::Held) => return Ok(PayOutcome::RecordHeld { attempt_id }),
            Ok(Written::Divergent(record)) => {
                return Ok(PayOutcome::RecordDivergent { attempt_id, record })
            }
            // A batch the client kept, refused about the caller before it was sent, still goes
            // once the app is current again: the entry stays, as for any outcome unwritten,
            // so a refusal of that batch later leaves it for a resume to write again.
            Err(error)
                if error.kept_batch().is_none() && error.code() == Some(ErrorCode::NotCurrent) =>
            {
                // An app that isn't current can write nothing (Paykit spec 7.3).
                self.set_stage(entry, Stage::Finished).await?;
                return Ok(if paid {
                    PayOutcome::PaidUnreported { attempt_id }
                } else {
                    PayOutcome::Failed {
                        attempt_id,
                        reason: "the app isn't current".into(),
                    }
                });
            }
            // Money moved: a failure to write it is never the payment's error (Paykit spec
            // 7.2); the journal keeps the proof for a resume.
            Err(_) if paid => return Ok(PayOutcome::PaidOutcomeUnwritten { attempt_id }),
            Err(error) => return Err(error),
        }
        if !paid {
            return Ok(PayOutcome::Failed {
                attempt_id,
                reason: "the payment method reported a failure".into(),
            });
        }
        if let Some(msg_id) = entry.proof_msg_id.clone() {
            // After the payment and its outcome's batch: a delivery failure leaves the proof
            // queued for the next flush, and decides nothing about paying.
            let _ = self.app.flush_message(&msg_id).await;
        }
        Ok(PayOutcome::Paid {
            attempt_id,
            proof_msg_id: entry.proof_msg_id.clone(),
        })
    }

    /// Step 3 (Paykit spec 7.2): `done` (basis `owner`) or `failed` on the attempt, and the
    /// payer's activity record with the acceptance sent with the payment and the proof, in
    /// one batch, which queues the proof (step 4) and moves the journal to `Finished` once
    /// applied. An attempt already resolved, by a receipt or another app, needs no claim
    /// entry. After `409 conflict` the claim and the record are read again and the batch is
    /// built again on them, unless the journal shows an earlier batch applied meanwhile.
    ///
    /// `done` never goes without the record's update: an absent or deleted record is
    /// rebuilt from the journal ([`Paykit::outcome_record`]), and one whose value doesn't
    /// read holds the outcome ([`Written::Held`]) unless `overwrite`, the user's repair.
    pub(crate) async fn write_outcome(
        &mut self,
        entry: &mut JournalEntry,
        paid: bool,
        overwrite: bool,
    ) -> Result<Written> {
        if paid {
            // A payment made waits for the user's confirmation of a divergent payer record,
            // before its acceptance is released: nothing of it goes over that record until
            // then (Paykit spec 6).
            let record = self.activity_key(&ActivityTarget::payer(entry.natural_key.clone()))?;
            if self.pause_at(&record).await?.write_waits() {
                return Ok(Written::Divergent(record));
            }
            // Every path that writes a payment made goes through here: its acceptance is
            // sendable, if it isn't yet, before the batch places it in the record (Paykit
            // spec 7.2 steps 2, 3). It never holds back the outcome, nor is it dropped: one
            // that can't be queued now leaves the entry at `AcceptanceOwed` below, which a
            // resume finishes ([`Paykit::finish_owed_acceptance`]).
            if let Err(error) = self.release_acceptance(entry, None).await {
                if !entry.owes_acceptance()? {
                    return Err(error);
                }
            }
        }
        let attempt_id = entry.attempt()?;
        let attempt = attempt_id.to_bytes();
        let target = ActivityTarget::payer(entry.natural_key.clone());
        for _ in 0..OUTCOME_TRIES {
            // A batch kept from before may have been applied since: it finished the entry.
            // Read bound to its key and to the entry's claim: a value of another attempt or
            // claim at its key is a corrupt store, never this payment finished.
            if let Some(kept) = self
                .journal_entry_on(&attempt_id, &entry.obligation)
                .await?
            {
                if matches!(kept.stage, Stage::Finished | Stage::AcceptanceOwed) {
                    *entry = kept;
                    return Ok(Written::Done);
                }
            }
            // The record's update is settled first: without it nothing is written, `done`
            // included (Paykit spec 7.2 step 3).
            let proof = if paid {
                Some(self.sign_proof(entry)?)
            } else {
                None
            };
            let record_write = match self
                .outcome_record(entry, &target, paid, proof.as_ref(), overwrite)
                .await?
            {
                OutcomeRecord::Write(write) => Some(*write),
                OutcomeRecord::Unchanged => None,
                OutcomeRecord::Held => {
                    if !entry.record_held {
                        entry.record_held = true;
                        self.save(entry).await?;
                    }
                    return Ok(Written::Held);
                }
                OutcomeRecord::Divergent(record) => return Ok(Written::Divergent(record)),
            };
            let step = match entry.step {
                Some(step) => Some(step),
                None => match self.attempt_state(entry).await? {
                    Some((step, Outcome::Open | Outcome::Unknown)) => Some(step),
                    _ => None,
                },
            };
            let mut batch = OpBatch::new();
            if let Some(step) = step {
                let op = if paid {
                    ClaimOp::done(entry.obligation, step, attempt, Basis::Owner, None)?
                } else {
                    ClaimOp::failed(entry.obligation, step, attempt)
                };
                batch = batch
                    .op(op.kind())
                    .obligation_deadline(entry.obligation, own_deadline(entry.deadline));
            }
            if let Some(write) = record_write {
                batch = batch.entry(write);
            }
            let mut finished = entry.clone();
            // An acceptance still owed keeps the entry where a resume visits it: the outcome
            // is written, the acceptance not yet sent or placed.
            finished.stage = if paid && entry.owes_acceptance()? {
                Stage::AcceptanceOwed
            } else {
                Stage::Finished
            };
            finished.record_held = false;
            finished.proof_msg_id = proof.as_ref().map(|p| p.msg_id().to_owned());
            batch = batch.local(journal_write(&finished)?);
            if batch.is_empty() {
                // Nothing to write at the switch: the proof is queued alone.
                if let Some(proof) = proof {
                    self.app.queue_signed(proof).await?;
                }
                self.save(&finished).await?;
                *entry = finished;
                return Ok(Written::Done);
            }
            if let Some(proof) = proof {
                batch = batch.queue(proof);
            }
            match self.app.submit_batch(batch).await {
                Ok(_) => {
                    *entry = finished;
                    return Ok(Written::Done);
                }
                // A repair builds on the version the user saw: one changed since is read
                // again and decided again, never overwritten.
                Err(error) if overwrite && error.code() == Some(ErrorCode::Conflict) => {
                    self.app.read_record(&target.record_target()).await?;
                    return Err(PaykitError::Conflict(
                        "the activity record changed since it was seen".into(),
                    ));
                }
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    entry.step = None;
                    self.app.read_record(&target.record_target()).await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(PaykitError::Conflict(
            "the claim or the activity record kept changing".into(),
        ))
    }

    /// The payer's activity record after the payment (Paykit spec 7.2 step 3): the
    /// acceptance sent with it and the proof, each in its role; a payment without a request
    /// that failed is marked so.
    ///
    /// For a payment made, `done` goes only with this update, since for an obligation
    /// without a deadline the record is what stops a second payment once the claim's
    /// tombstone has gone:
    ///
    /// - **absent or deleted:** the record is rebuilt from the journal, the request kept
    ///   with the entry, or a payment without a request's own members, with the acceptance
    ///   and the proof, and written as a create (`{0, 0}`, which the switch refuses if a
    ///   record is there by then). A delete isn't a write that drops a role (Paykit wire
    ///   5.3, "Seen roles"); a reader that saw a role the rebuilt record lacks, such as a
    ///   receipt, ignores and flags it, and keeps what it saw.
    /// - **a value that doesn't read,** such as one a faulty own app wrote, or one sealed at
    ///   an epoch whose key the app lacks: [`OutcomeRecord::Held`]. Overwriting it unasked
    ///   could drop a role another own app wrote that this one can't see, and every reader
    ///   that saw it would ignore the write (Paykit wire 5.3, "Seen roles"), so the user
    ///   decides ([`Paykit::repair_activity_record`], `overwrite`): the rebuilt record then
    ///   replaces it, built on the version seen.
    /// - an entry that can't rebuild its record (one journaled before requests were kept)
    ///   holds too.
    ///
    /// A failure moves no money, and its `failed` is what lets a payment be made again: it
    /// goes with the record's update when the record reads, and alone otherwise.
    async fn outcome_record(
        &mut self,
        entry: &JournalEntry,
        target: &ActivityTarget,
        paid: bool,
        proof: Option<&SignedMessage>,
        overwrite: bool,
    ) -> Result<OutcomeRecord> {
        let own = self.identity();
        let key = self.activity_key(target)?;
        let held = self.app.shared_record(&key).await?;
        // A value only waiting for its scope key opens once the key comes: the outcome waits
        // for it, never offered as a repair over a value that only isn't open yet.
        if held
            .as_ref()
            .is_some_and(|r| crate::hold::awaits_key(&r.content))
        {
            return Err(crate::hold::value_awaits_key());
        }
        // A divergent record the user hasn't confirmed takes no own write but a stop (Paykit
        // spec 6): a payment made waits, `done` never going without the record's update; a
        // failure's `failed` goes alone, its record left as it is.
        if let Some(current) = &held {
            if self.pause_of(current).await?.write_waits() {
                return Ok(if paid {
                    OutcomeRecord::Divergent(key)
                } else {
                    OutcomeRecord::Unchanged
                });
            }
        }
        let read = match held.as_ref().map(|r| &r.content) {
            None | Some(RecordContent::Deleted) => None,
            Some(content) => match activity::read(content.value(), &own, ActivitySide::Payer) {
                Some(record) => Some(record),
                None if paid && overwrite => None,
                None if paid => return Ok(OutcomeRecord::Held),
                None => return Ok(OutcomeRecord::Unchanged),
            },
        };
        // A rebuilt record is written whatever the outcome places in it.
        let (mut record, mut changed) = match read {
            Some(record) => (record, false),
            None if !paid => return Ok(OutcomeRecord::Unchanged),
            None => match self.rebuilt_record(entry)? {
                Some(record) => (record, true),
                None => return Ok(OutcomeRecord::Held),
            },
        };
        changed |= self.place_acceptance(&mut record, entry)?;
        if let Some(proof) = proof {
            changed |= activity::place(&mut record, Role::Proof, self.own_copy(proof)?);
        }
        // A payment made after `failed` was written on its claim: the record's informational
        // `status` shows it for display; no reader decides by it (Paykit wire 5.3).
        let after_failed = Some(crate::receive::PAID_AFTER_FAILED);
        if paid && entry.failed_on_claim && record.status.as_deref() != after_failed {
            record.status = after_failed.map(str::to_owned);
            changed = true;
        }
        if !paid && record.unrequested.is_some() && record.status.as_deref() != Some("failed") {
            record.status = Some("failed".into());
            changed = true;
        }
        if !changed {
            return Ok(OutcomeRecord::Unchanged);
        }
        let value = record.to_json()?;
        Ok(OutcomeRecord::Write(Box::new(
            self.app
                .record_op(&target.record_target(), Some(&value), Vec::new())
                .await?,
        )))
    }

    /// Place the acceptance `entry` journaled, sent with its payment, in `record`'s
    /// response role (Paykit spec 7.2 step 3). Whether the record changed.
    pub(crate) fn place_acceptance(
        &self,
        record: &mut ActivityRecord,
        entry: &JournalEntry,
    ) -> Result<bool> {
        // Only an acceptance that went out: one journaled for a payment that never ran
        // isn't the payer's (Paykit spec 7.2 step 2). It is placed under the certificate it
        // was signed under, not a later renewal's.
        if !entry.acceptance_released {
            return Ok(false);
        }
        let Some(stored) = entry.acceptance_message(&self.identity(), self.app.cert())? else {
            return Ok(false);
        };
        Ok(activity::place(record, Role::Response, stored))
    }

    /// The payer's activity record of `entry`'s payment, rebuilt from the journal without
    /// its acceptance and proof, which the outcome places: the request kept with the entry,
    /// or a payment without a request's own members (Paykit wire 5.3). `None` when the entry
    /// keeps no request it can be rebuilt from.
    pub(crate) fn rebuilt_record(&self, entry: &JournalEntry) -> Result<Option<ActivityRecord>> {
        let own = self.identity();
        let mut record = activity::empty(ActivitySide::Payer, entry.payee);
        match entry.request_hash()? {
            Some(hash) => {
                let Some(kept) = &entry.request else {
                    return Ok(None);
                };
                let request = kept.stored(&own)?;
                if request.hash() != hash || request.sender() != entry.payee {
                    return Err(PaykitError::corrupt());
                }
                activity::place(&mut record, Role::Request, request);
            }
            None => {
                record.attempt_id = Some(entry.attempt()?);
                record.unrequested = Some(crate::receive::unrequested_of(
                    &entry.destination,
                    &entry.amount.amount()?,
                )?);
                record.status = Some("paying".into());
            }
        }
        Ok(Some(record))
    }

    /// The claim's step and this attempt's outcome, read from the switch; `None` when the
    /// claim doesn't hold the attempt.
    async fn attempt_state(&mut self, entry: &JournalEntry) -> Result<Option<(u64, Outcome)>> {
        let attempt = entry.attempt()?.to_bytes();
        let Some(read) = self.app.read_claim(&entry.obligation).await? else {
            return Ok(None);
        };
        Ok(read
            .view
            .attempt(&attempt)
            .filter(|a| a.owner == self.app_id())
            .map(|a| (read.view.step, a.outcome)))
    }

    /// Whether the client still keeps the batch that carries `entry`'s attempt, to send
    /// again: its claim can't show the attempt until the batch is answered, so the entry is
    /// left as it is. Anything else reads the claim: a batch applied or dropped, and an
    /// attempt the client knows no batch of, which never says it was never written. A batch
    /// ended past the client's retention, kept before a restore, or written another way is
    /// unknown to the client, and only the claim tells whether the attempt is on it.
    async fn attempt_still_kept(&self, entry: &JournalEntry) -> Result<bool> {
        let attempt = ClaimAttempt {
            claim_key: entry.obligation,
            attempt_id: entry.attempt()?.to_bytes(),
        };
        Ok(matches!(
            self.app.kept_attempt(attempt).await?,
            Some(KeptBatch {
                status: KeptBatchStatus::Kept { .. },
                ..
            })
        ))
    }

    /// The proof (Paykit spec 7.2 step 4, Paykit wire 4.10), signed for the outcome's batch.
    fn sign_proof(&self, entry: &JournalEntry) -> Result<SignedMessage> {
        let request_hash = entry.request_hash()?;
        let proof = PaymentProof {
            attempt_id: entry.attempt()?,
            billing_period: None,
            payment_endpoint_identifier: entry.destination.endpoint_id()?,
            payment_app_id: entry.destination.app_id,
            list_version: entry.destination.list_version,
            amount: request_hash
                .is_none()
                .then(|| entry.amount.amount())
                .transpose()?,
            allowance_id: None,
            label: entry.label()?,
            payment_reference: None,
            proof: entry.rail_proof.clone().ok_or_else(PaykitError::corrupt)?,
        };
        let refs: Vec<[u8; 32]> = request_hash.into_iter().collect();
        let body = proof.to_body(refs.len())?;
        self.sign(entry.payee, Kind::PaymentProof, refs, body)
    }

    /// Finish the payments a crash, a lost answer or a pending payment method left in the
    /// journal (Paykit spec 7.2): before money could move, an own attempt still open is
    /// written `failed`; once paying, the method's history decides, and a dispatch is never
    /// carried out again unless the method is idempotent on the attempt ID; a restored entry
    /// is resolved only from a history that survives a restore. A repeat on an idempotent
    /// method passes every check a first dispatch does.
    ///
    /// **Journal pruning** (Paykit spec 7.1, 7.2; Switch spec 6.3). After every payment's own
    /// work, a resume drops the `Finished` and `Abandoned` entries past their claim's window:
    /// one with a deadline from that deadline plus 30 days ([`RECONCILIATION_MS`]), the time
    /// the switch keeps the claim's tombstone, and one without from 400 days after it was
    /// journaled ([`MAX_CLAIM_AGE_MS`]). A bound passes only once it lies before the earlier
    /// of the highest service time verified and the device clock plus 24 hours, so no
    /// far-future signed time passes one early, and without a verified service time nothing
    /// is dropped. What a later reconcile, a restore's included, needs is never dropped: an
    /// entry still open or [`Stage::AcceptanceOwed`], one whose dispatch the payment method
    /// reported failed while its history may still show it paid, and one paid after `failed`
    /// was written, waiting for its receipt. A dropped payment leaves [`Paykit::payments`],
    /// and its acknowledged notices go with it; the payer's activity record, not the
    /// journal, shows it and stops a second one (Paykit spec 6, 7.2, "The record shows it
    /// paid").
    ///
    /// # Errors
    ///
    /// [`Refusal::IdentityUnavailable`] when a repeat is due and the own `identity.json`
    /// isn't confirmed in the last 60 s; the client's errors; the rail's errors.
    pub async fn resume_payments(&mut self) -> Result<Vec<PayOutcome>> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.resume_payments_inner()).await
    }

    async fn resume_payments_inner(&mut self) -> Result<Vec<PayOutcome>> {
        self.refresh_own()?;
        // While a wider certificate's scope keys are missing, the resume goes on only on the
        // payer's side it can read: with the `pay` key held and the payer's records and
        // claims read as before, its writes are built on versions it holds. Otherwise it
        // asks the method's history only, journals what it says, and writes nothing to the
        // switch, as without `pay`, until the keys come.
        // A restore's copy not listed yet lacks every record: while keys wait, which leaves it
        // unlisted, the payer's side waits too.
        let keys = self.scope_keys_awaited().await?;
        let payer_waits = self.payer_waits(keys).await?;
        if keys.is_none() {
            self.records_known().await?;
        }
        // Batches kept from before go first, so the claim read below shows those sent and
        // the journal holds what their answers settled. One the client still can't send
        // stays kept: its attempt isn't on the claim yet, so its entry is left `Recorded`
        // below, until the batch is sent or the client drops it. The flush sends only bytes
        // signed earlier, kept batches and queued messages, and builds nothing on the copy,
        // so it goes while the payer's side waits too.
        self.app.flush_outbox().await?;
        // Cancellations kept and not yet placed go into their records (Paykit spec 8.1),
        // which they build on: not while the payer's side waits.
        if !payer_waits {
            self.place_kept_cancellations().await?;
        }
        let mut out = Vec::new();
        for mut entry in self.journal().await? {
            // An outcome written whose acceptance is still owed: queued and placed now.
            // One that fails now stays owed for the next resume, and stops no other entry:
            // those after it are still resolved from the method's history.
            if entry.stage == Stage::AcceptanceOwed {
                if !payer_waits {
                    let _ = self.finish_owed_acceptance(&mut entry).await;
                }
                continue;
            }
            let held = !self.holds("pay") || payer_waits;
            if !held {
                // Under `pay` again, the payment is held without it no more: that notice goes,
                // so a later certificate without `pay` tells the host again.
                self.drop_notice(NoticeKind::HeldWithoutPay, &entry).await?;
            }
            if !entry.open() {
                continue;
            }
            let attempt_id = entry.attempt()?;
            // Without `pay` the app reads and writes no claim and no payer record (the
            // service refuses both, `403 permission_missing`), and makes no payment
            // ([`Paykit::final_payment_check`]): every open entry waits for an app holding
            // it, its proof kept if it has one, and the other entries go on.
            if held {
                out.push(self.held_without_pay(&mut entry).await?);
                continue;
            }
            match entry.stage {
                Stage::Recorded | Stage::Attempted | Stage::Paying if entry.restored => {
                    match self.attempt_state(&entry).await? {
                        Some((step, Outcome::Open | Outcome::Unknown)) => {
                            entry.step = Some(step);
                            entry.stage = Stage::Paying;
                            out.extend(self.resolve_dispatched(entry).await?);
                        }
                        // Restored, it may have been dispatched (Paykit spec 7.2).
                        Some((_, outcome)) => self.finish_resolved(&mut entry, outcome).await?,
                        None => self.set_stage(&mut entry, Stage::Abandoned).await?,
                    }
                }
                Stage::Recorded if self.attempt_still_kept(&entry).await? => {}
                Stage::Recorded | Stage::Attempted => match self.attempt_state(&entry).await? {
                    // No payment started: the open attempt is written `failed`.
                    Some((step, Outcome::Open | Outcome::Unknown)) => {
                        entry.step = Some(step);
                        self.write_outcome(&mut entry, false, false).await?;
                        out.push(PayOutcome::Failed {
                            attempt_id,
                            reason: "Paykit spec 7.2: no payment started".into(),
                        });
                    }
                    // Known unsubmitted: this app never called the method.
                    Some((_, outcome)) => self.finish_resolved(&mut entry, outcome).await?,
                    None => self.set_stage(&mut entry, Stage::Abandoned).await?,
                },
                Stage::Paying => {
                    out.extend(self.resolve_dispatched(entry).await?);
                }
                Stage::RailPaid | Stage::RailFailed => {
                    out.push(self.report(&mut entry).await?);
                }
                Stage::AcceptanceOwed | Stage::Finished | Stage::Abandoned => {}
            }
        }
        if self.holds("pay") && !payer_waits {
            out.extend(self.recheck_failed().await?);
        }
        self.prune_journal().await?;
        // Last, after every payment's own work: what can't be delivered now stays queued.
        let _ = self.app.flush_outbox().await;
        Ok(out)
    }

    /// An own attempt the claim shows resolved already, by the user in another app or by
    /// evidence. The acceptance goes only for a payment this app may have dispatched
    /// ([`JournalEntry::may_have_dispatched`]): `done` then says it was made, the user's word
    /// standing where the method's history would, so an acceptance owed is queued and placed
    /// ([`Paykit::finish_owed_acceptance`], Paykit spec 7.2 step 2), one queued earlier and
    /// not yet placed included; one that can't be now leaves the entry for the next resume.
    /// An entry known unsubmitted, or `failed`, finishes with none: a payee never sees an
    /// acceptance of a payment that never ran.
    async fn finish_resolved(&mut self, entry: &mut JournalEntry, outcome: Outcome) -> Result<()> {
        if entry.may_have_dispatched()
            && outcome == Outcome::Done
            && entry.may_place_acceptance()?
        {
            let _ = self.finish_owed_acceptance(entry).await;
            return Ok(());
        }
        self.set_stage(entry, Stage::Finished).await
    }

    /// An open entry under a certificate without `pay`, which reads and writes no claim and
    /// no payer record: a dispatch that may have been made asks the method's history alone,
    /// so a payment made is journaled with its proof (`RailPaid`) and one that failed as such
    /// (`RailFailed`), each for an app holding `pay` to report; nothing else is touched. The
    /// host is told why the payment waits ([`NoticeKind::HeldWithoutPay`]).
    async fn held_without_pay(&mut self, entry: &mut JournalEntry) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        self.raise_notice(NoticeKind::HeldWithoutPay, entry).await?;
        if entry.stage == Stage::Paying || entry.restored && entry.may_have_dispatched() {
            match self.rail.payment(attempt_id).await? {
                Some(RailOutcome::Paid(RailPaid {
                    proof, paid_at_ms, ..
                })) => {
                    entry.rail_proof = Some(proof);
                    entry.paid_at_ms = paid_at_ms;
                    self.set_stage(entry, Stage::RailPaid).await?;
                }
                Some(RailOutcome::Failed) => {
                    entry.failed_by_method = true;
                    self.set_stage(entry, Stage::RailFailed).await?;
                }
                Some(RailOutcome::Pending) | None => {}
            }
        }
        Ok(if entry.rail_proof.is_some() {
            PayOutcome::PaidOutcomeUnwritten { attempt_id }
        } else {
            PayOutcome::Pending { attempt_id }
        })
    }

    /// Drop the journal entries past their claim's reconciliation window (Paykit spec 7.1,
    /// 7.2; Switch spec 6.3): `Finished` and `Abandoned` ones, from their deadline plus the
    /// 30 days the switch keeps the claim's tombstone after it ([`RECONCILIATION_MS`]),
    /// or, without a deadline, from 400 days after they were journaled
    /// ([`MAX_CLAIM_AGE_MS`]). A claim pinned to none is kept 400 days after its last entry,
    /// which comes after the journaling, so it may outlive the entry: a finished entry
    /// decides nothing then, since the payer's activity record, not the journal, shows the
    /// payment and stops a second one (Paykit spec 6, 7.2, "The record shows it paid").
    ///
    /// What a later reconcile, a restore's included, needs stays: every entry still open
    /// or owing its acceptance, one whose dispatch the payment method reported failed while
    /// its history may still show it paid, and one paid after `failed` was written, whose
    /// receipt the user is told about. A bound passes as [`bound_passed`] says. An entry's
    /// acknowledged notices go with it.
    ///
    /// Two of those have no bound of their own: a payment made after `failed` was written
    /// waits for its receipt for as long as no receipt comes, and a dispatch the method
    /// reported failed is asked about for as long as the method's history keeps it, without
    /// end for a method whose history is kept for good. Each is one small entry.
    async fn prune_journal(&mut self) -> Result<()> {
        let service = self.service_time();
        let now = self.now();
        let mut tx = state::Tx::default();
        for entry in self.journal().await? {
            let Some(bound) = prune_bound(
                entry.stage,
                entry.failed_by_method || entry.failed_on_claim,
                entry.deadline,
                entry.journaled_at_ms,
            ) else {
                continue;
            };
            if bound_passed(bound, service, now) {
                let attempt = entry.attempt()?;
                tx.delete(&key::journal(&attempt));
                // Its acknowledged notices go with it; one not acknowledged stays for the
                // host, and goes when acknowledged.
                for kind in [NoticeKind::PaidAfterFailed, NoticeKind::HeldWithoutPay] {
                    let notice = key::notice(&attempt, kind);
                    if self
                        .kept_notice(&notice)
                        .await?
                        .is_some_and(|kept| kept.acknowledged)
                    {
                        tx.delete(&notice);
                    }
                }
            }
        }
        self.local_commit(tx).await
    }

    /// Ask the payment method's history again about the dispatches it reported failed, while
    /// that history keeps them (Paykit spec 7.2, "What a payment method declares"): an owner
    /// that finds a payment it reported failed completed after all sends its proof, and the
    /// payee's receipt corrects the claim. Such a payment is reported as any other, its
    /// activity record and its proof in one batch; a dispatch the history no longer keeps
    /// isn't asked about again.
    async fn recheck_failed(&mut self) -> Result<Vec<PayOutcome>> {
        let declaration = self.rail.declaration();
        let mut out = Vec::new();
        for mut entry in self.journal().await? {
            if entry.stage != Stage::Finished || !entry.failed_by_method {
                continue;
            }
            let kept = declaration.history
                && retained(
                    declaration.history_retention_ms,
                    entry.journaled_at_ms,
                    self.now(),
                );
            if !kept {
                entry.failed_by_method = false;
                self.save(&entry).await?;
                continue;
            }
            let Some(RailOutcome::Paid(RailPaid {
                proof, paid_at_ms, ..
            })) = self.rail.payment(entry.attempt()?).await?
            else {
                continue;
            };
            entry.failed_by_method = false;
            // The attempt is `failed` on the claim: no claim entry is written, and the
            // payee's receipt settles it by evidence (Switch spec 10.2 row 3), or, once the
            // claim is gone, tells the user it was paid.
            entry.failed_on_claim = true;
            entry.step = None;
            entry.rail_proof = Some(proof);
            entry.paid_at_ms = paid_at_ms;
            // Opened again, so the outcome's batch is built afresh rather than taken as done.
            self.set_stage(&mut entry, Stage::RailPaid).await?;
            out.push(self.report(&mut entry).await?);
        }
        Ok(out)
    }

    /// Resolve an attempt whose payment may have been dispatched (Paykit spec 7.2, "What a
    /// payment method declares"): any history record resolves it; an empty history means it
    /// never started only within the history's retention, and for a restored entry only if
    /// the history survives a restore; otherwise an idempotent method is asked again within
    /// its retention, while the claim still shows this app's attempt open, and anything else
    /// stays "may be paid". `None` when the claim shows the attempt resolved already, by the
    /// user or by evidence: the entry is finished, with no outcome of its own.
    pub(crate) async fn resolve_dispatched(
        &mut self,
        mut entry: JournalEntry,
    ) -> Result<Option<PayOutcome>> {
        let attempt_id = entry.attempt()?;
        if let Some(outcome) = self.rail.payment(attempt_id).await? {
            return self.take_outcome(&mut entry, outcome, None).await.map(Some);
        }
        let declaration = self.rail.declaration();
        let now = self.now();
        let within = |retention: Option<u64>| retained(retention, entry.journaled_at_ms, now);
        if declaration.history
            && within(declaration.history_retention_ms)
            && (!entry.restored || declaration.history_survives_restore)
        {
            entry.stage = Stage::Paying;
            return self
                .take_outcome(&mut entry, RailOutcome::Failed, None)
                .await
                .map(Some);
        }
        if !entry.restored && declaration.idempotent && within(declaration.idempotency_retention_ms)
        {
            // A repeat may execute money: only while the claim still shows this app's own
            // attempt open. One resolved meanwhile is never carried out again, and one in
            // doubt (`unknown`, or gone from the claim) stays so (Paykit spec 7.2, 7.3).
            // The monotonic clock before the read, with the service time verified once it is
            // answered: their sum later never falls short of service time, the read's own
            // time included.
            let read_started = std::time::Instant::now();
            let Some(claim) = self.app.read_claim(&entry.obligation).await? else {
                return Ok(Some(PayOutcome::Uncertain { attempt_id }));
            };
            let read_at = (self.service_time().unwrap_or(0), read_started);
            let claim = claim.view;
            let me = self.app_id();
            let held = claim
                .attempt(&attempt_id.to_bytes())
                .filter(|a| a.owner == me);
            // The attempt's age and orphan window, for the bound on a repeat (Switch spec
            // 10.2); unused unless the attempt is open.
            let (opened_at_ms, orphan_after_s) =
                held.map_or((0, 0), |a| (a.opened_at, a.orphan_after));
            match held.map(|a| a.outcome) {
                Some(Outcome::Open) => entry.step = Some(claim.step),
                Some(outcome @ (Outcome::Done | Outcome::Failed)) => {
                    self.finish_resolved(&mut entry, outcome).await?;
                    return Ok(None);
                }
                Some(Outcome::Unknown) | None => {
                    return Ok(Some(PayOutcome::Uncertain { attempt_id }))
                }
            }
            // The user decided to pay at the first dispatch: a request the payee cancelled
            // since, or one this app keeps a stop for, isn't paid again. The repeat is
            // withheld and nothing is written: the first dispatch may have paid, so the
            // attempt stays open and "may be paid", never `failed`, until the method's
            // history or a receipt tells (Paykit spec 7.2, 8.1).
            if self.repeat_withheld(&entry).await? {
                return Ok(Some(PayOutcome::Uncertain { attempt_id }));
            }
            let doc = match self.repeat_held(&entry, &claim).await? {
                Ok(doc) => doc,
                Err(held) => return Ok(Some(held)),
            };
            // The retention is judged again by the final payment check, after its await and
            // just before the call: the checks awaited the network meanwhile.
            let repeat = Dispatch::Repeat {
                idempotency_retention_ms: declaration.idempotency_retention_ms,
                opened_at_ms,
                orphan_after_s,
                longest_completion_ms: declaration.longest_completion_ms,
                read_at,
            };
            return self
                .dispatch(entry, doc, Initiator::Automatic, None, repeat, None)
                .await
                .map(Some);
        }
        Ok(Some(PayOutcome::Uncertain { attempt_id }))
    }

    /// Whether a repeat of `entry`'s dispatch is withheld because its request may no longer
    /// be paid (Paykit spec 8.1): this app keeps a stop for it, or the request record the
    /// switch holds, read again, shows it cancelled or rejected, or isn't there (a `404`
    /// included, whatever copy this app keeps) or doesn't read as the request, a newer write
    /// this app refused included, so nothing shows it still open; or the request is held
    /// after a restore or a `missed` item until the user confirms it (Paykit spec 7.2,
    /// [`Paykit::confirm_possibly_cancelled`]). A payment without a request has nothing a
    /// payee cancels.
    async fn repeat_withheld(&mut self, entry: &JournalEntry) -> Result<bool> {
        // A repeat is an automatic payment: none while its request record or its list's
        // record is divergent, or paused since it was seen so (Paykit spec 6). The view held
        // is taken before the record is read again, and the one read after it.
        if self.divergent_withholds(entry).await? {
            return Ok(true);
        }
        if entry.request_hash()?.is_none() {
            return Ok(false);
        }
        let target = ActivityTarget::payer(entry.natural_key.clone());
        if self.kept_stop(&target).await? {
            return Ok(true);
        }
        Ok(match self.fresh_request(&target).await? {
            Fresh::Held(fresh) => {
                cancelled(&fresh)
                    || rejected(&fresh)
                    || self.held(&self.activity_key(&target)?, &fresh).await?
                    || self.divergent_withholds(entry).await?
            }
            Fresh::Missing | Fresh::Unreadable => true,
        })
    }

    /// Whether `entry`'s request record, or the `payment_lists` record of the list its
    /// destination came from, blocks an automatic payment ([`crate::divergent::Pause`]).
    async fn divergent_withholds(&mut self, entry: &JournalEntry) -> Result<bool> {
        let mut records = Vec::new();
        if entry.request_hash()?.is_some() {
            records.push(self.activity_key(&ActivityTarget::payer(entry.natural_key.clone()))?);
        }
        if entry.destination.source()? == DestinationSource::List {
            records.push(
                self.app
                    .record_key(&list_target(&entry.payee, &entry.destination.app_id))?,
            );
        }
        for record in records {
            if self.pause_at(&record).await?.blocks_automatic() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The final payment check, the last thing before the payment method's call on every
    /// path that reaches it, a first dispatch and a repeat at a resume or a recovery (Paykit
    /// spec 4, 7.2). In order:
    ///
    /// 0. for an automatic payment, the deny list, read again: a blocked payee, or a list
    ///    that can't be read, holds it ([`FinalCheck::Hold`], Paykit spec 8.1);
    /// 1. a switch lookup kept for the payee whose answer has come is taken, so a late
    ///    answer that removes the destination's app is acted on (Switch spec 4.3), and one
    ///    the client refuses, such as an equivocation, stops the payment
    ///    ([`FinalCheck::Refused`], Switch wire 3.3);
    /// 2. the destination, against the payee's `identity.json` that gives: its carrier
    ///    carries it and hasn't expired, and its app is current, not removed, with the
    ///    certificate the carrier came with;
    /// 3. the payee's copy may still be acted on: read in the last 10 minutes for a
    ///    request's or a list's endpoint, by the switch's rules for a public endpoint;
    /// 4. the own `identity.json` is confirmed in the last 60 s, and lists this app at the
    ///    attempt's service and generation;
    /// 5. for a repeat, the method's idempotency retention hasn't ended, and the repeat can
    ///    complete before its attempt is orphaned ([`FinalCheck::Lapsed`]); for a first
    ///    dispatch, the attempt is within its pre-dispatch budget;
    /// 6. the app may still make the attempt: no alarm or restore question raised meanwhile,
    ///    for an automatic payment the clocks within the tolerance, and its certificate
    ///    holds `pay`, so an app a renewal reduced moves no money on any path, a repeat at a
    ///    resume or a recovery included ([`FinalCheck::Hold`]: nothing is paid, the attempt
    ///    stays open for an app that holds it).
    ///
    /// Checks 2 to 6 run on the clock after the one await ([`Paykit::final_gates`]). A
    /// repeat's call follows them with no await; a first dispatch's follows the journal
    /// write of `Paying`, after which the fence ([`Paykit::payment_fence`]) runs them all
    /// again. Anything but [`FinalCheck::Pay`]: nothing is paid; a copy past its age is
    /// looked up again by the next check, at a resume. Returns the payee's copy the checks
    /// judged, for the fence.
    pub(crate) async fn final_payment_check(
        &mut self,
        entry: &JournalEntry,
        payee: PeerIdentity,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        kind: Dispatch,
    ) -> Result<(FinalCheck, PeerIdentity)> {
        // No automatic payment to a blocked payee (Paykit spec 8.1), on any automatic path,
        // a resume's repeat included: the user's confirmation was for the first dispatch. A
        // deny list that can't be read holds it too. Judged before the await below, after
        // which nothing time-bound is: the list changes only by the user's or the switch's
        // write, which a later check reads again.
        if by == Initiator::Automatic && self.denied(&entry.payee).await.unwrap_or(true) {
            return Ok((FinalCheck::Hold, payee));
        }
        let payee = match self.take_kept_lookup(&entry.payee).await? {
            Ok(Some(answered)) => answered,
            Ok(None) => payee,
            Err(refusal) => return Ok((FinalCheck::Refused(refusal), payee)),
        };
        let check = self.final_gates(entry, &payee, by, confirmed, kind)?;
        Ok((check, payee))
    }

    /// The fence after the journal write of `Paying` (Paykit spec 7.2): that commit is an
    /// await, during which a lookup's answer can come and a freshness limit pass, so every
    /// gate of the final payment check is judged again here, with no await of its own and
    /// none after it before the payment method's call:
    ///
    /// - a switch lookup kept for the payee that has answered meanwhile, polled once without
    ///   waiting: its answer may remove the destination's app or show equivocation, and
    ///   taking it is an await, so it stops the payment and is kept for the next check;
    /// - every gate of [`Paykit::final_gates`], on the clock now.
    ///
    /// The kept stop and the restore hold aren't judged again: Paykit's own state, which
    /// only this app's own calls change (a receive, a placement, a confirmation), and none
    /// can run while the payment holds the app.
    fn payment_fence(
        &mut self,
        entry: &JournalEntry,
        payee: &PeerIdentity,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        kind: Dispatch,
    ) -> Result<FinalCheck> {
        if self.kept_lookup_answered(&entry.payee) {
            return Ok(FinalCheck::Hold);
        }
        self.final_gates(entry, payee, by, confirmed, kind)
    }

    /// Whether a repeat made now completes, at the method's longest, before its attempt is
    /// orphaned (Switch spec 10.2): `now + longest_completion_ms < opened_at + orphan_after`,
    /// on the service clock. Service time now is taken late: the latest of the highest
    /// service time verified, the device clock plus the clock tolerance, and the service
    /// time verified at the claim read plus the monotonic time since (`read_at`), which a
    /// device clock set back meanwhile can't shorten. Past it another
    /// own app may soon resolve the attempt, `unknown` or `failed` with the user's
    /// confirmation (rows 5 and 6), while the repeat's payment could still run, so it isn't
    /// made, and the attempt stays "may be paid" as past the idempotency retention.
    ///
    /// A repeat has no journal write between its final payment check and its call, so no
    /// fence after it: this check, with the others, is the last before the call, and nothing
    /// awaits between them.
    fn repeat_completes_before_orphaned(
        &self,
        now: u64,
        read_at: (u64, std::time::Instant),
        opened_at_ms: u64,
        orphan_after_s: u64,
        longest_completion_ms: u64,
    ) -> bool {
        let (read_service_ms, read_instant) = read_at;
        let since_read = u64::try_from(read_instant.elapsed().as_millis()).unwrap_or(u64::MAX);
        let service_now = self
            .service_time()
            .unwrap_or(0)
            .max(now.saturating_add(CLOCK_TOLERANCE_MS))
            .max(read_service_ms.saturating_add(since_read));
        let orphaned_at = opened_at_ms.saturating_add(orphan_after_s.saturating_mul(1000));
        service_now.saturating_add(longest_completion_ms) < orphaned_at
    }

    /// A first dispatch's attempt is still within the pre-dispatch budget
    /// ([`crate::PRE_DISPATCH_BUDGET_MS`]): its age since just before it was sent, by the
    /// device's wall and monotonic clocks, with the clock tolerance added, is under the
    /// budget, and the
    /// own certificate hasn't expired by the service time that makes. `orphan_after` is at
    /// least the rail's longest time plus the budget ([`crate::orphan_floor_s`]), so within
    /// it no other own app can resolve the attempt as orphaned before its payment has run
    /// (Switch spec 10.2 rows 5 and 6). Past it nothing is submitted: the attempt is written
    /// `failed`, and a payment made after is a new decision.
    ///
    /// Residual: without a read, nothing here can see the owner's own revocation within the
    /// budget. An open attempt whose owner isn't current is orphaned at once, but rows 5 and
    /// 6 need the user's confirmation and flag the next attempt `possibly_duplicate`, which
    /// is never paid automatically (Paykit spec 7.3), and the signer warns before revoking
    /// an app that owns open attempts.
    fn within_budget(&self, answered: &Answered, now: u64) -> std::result::Result<(), Refusal> {
        let age = answered.age_ms(now).saturating_add(CLOCK_TOLERANCE_MS);
        if age >= crate::PRE_DISPATCH_BUDGET_MS {
            return Err(Refusal::NotNow(
                "Switch spec 10.2: the attempt is past its pre-dispatch budget".into(),
            ));
        }
        if self
            .app
            .cert()
            .expired_at(answered.opened_at_ms.saturating_add(age))
        {
            return Err(Refusal::NotNow(
                "Switch spec 3.2: the app's certificate expires within the pre-dispatch budget"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Checks 2 to 6 of the final payment check, with no await, on the clock now and the
    /// payee's copy `payee`, and for a first dispatch the pre-dispatch budget
    /// ([`Paykit::within_budget`]).
    fn final_gates(
        &self,
        entry: &JournalEntry,
        payee: &PeerIdentity,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        kind: Dispatch,
    ) -> Result<FinalCheck> {
        if !self.holds("pay") {
            return Ok(FinalCheck::Hold);
        }
        if let Err(refusal) = self.transfer_holds(entry)? {
            return Ok(FinalCheck::Refused(refusal));
        }
        let now = self.now();
        match kind {
            Dispatch::Repeat {
                idempotency_retention_ms,
                opened_at_ms,
                orphan_after_s,
                longest_completion_ms,
                read_at,
            } => {
                if !retained(idempotency_retention_ms, entry.journaled_at_ms, now) {
                    return Ok(FinalCheck::Lapsed);
                }
                if !self.repeat_completes_before_orphaned(
                    now,
                    read_at,
                    opened_at_ms,
                    orphan_after_s,
                    longest_completion_ms,
                ) {
                    return Ok(FinalCheck::Lapsed);
                }
            }
            Dispatch::First { answered } => {
                if let Err(refusal) = self.within_budget(&answered, now) {
                    return Ok(FinalCheck::Refused(refusal));
                }
            }
        }
        let Some(carrier) = entry.kept_carrier(&self.identity())? else {
            return Ok(FinalCheck::Hold);
        };
        let destination = self
            .destination_valid(
                &entry.payee,
                &entry.destination,
                &carrier,
                entry.request_hash()?.as_ref(),
                &payee.doc,
            )
            .is_ok();
        // An alarm or a restore question raised during any await holds the payment back
        // (Switch spec 10.1, 12.4).
        let clear = self.app.may_attempt(by == Initiator::Automatic).is_ok()
            && destination
            && self.payee_copy_usable(payee, Need::of(&carrier), by, confirmed)
            && self.own_identity_fresh()
            && self.own_identity_authorizes(entry).is_ok();
        Ok(if clear {
            FinalCheck::Pay
        } else {
            FinalCheck::Hold
        })
    }

    /// The checks before a repeat, the same as before a first dispatch (Paykit spec 7.2 step
    /// 1): a repeat may execute money, so an idempotency guarantee alone doesn't let it go.
    /// The app may make an automatic attempt now (no alarm, not in reconcile mode, the clocks
    /// within the tolerance), its own `identity.json` is confirmed within the last 60 s, and
    /// the destination passes its checks, for an automatic payment. `None` lets the repeat
    /// go; otherwise the attempt stays open, as nothing may be written `failed` once a
    /// payment may have been submitted: [`PayOutcome::Pending`] for what may pass later,
    /// [`PayOutcome::Uncertain`] for a destination refused, or one this entry can't check.
    ///
    /// # Errors
    ///
    /// [`Refusal::IdentityUnavailable`] when the own `identity.json` isn't confirmed: no
    /// payment of this app goes on then.
    async fn repeat_held(
        &mut self,
        entry: &JournalEntry,
        claim: &ClaimView,
    ) -> Result<std::result::Result<PeerIdentity, PayOutcome>> {
        let attempt_id = entry.attempt()?;
        let Some(carrier) = entry.kept_carrier(&self.identity())? else {
            return Ok(Err(PayOutcome::Uncertain { attempt_id }));
        };
        let gate = self
            .pay_gate(entry, &carrier, claim, Initiator::Automatic, None, true)
            .await?;
        Ok(Err(match gate {
            Ok(doc) => return Ok(Ok(doc)),
            // No payment of this app goes on without its own identity.json confirmed.
            Err(Held::OwnIdentity(refusal)) => return Err(refusal.into()),
            // What may pass later: the app held back, or the payee's identity.json not
            // confirmed yet.
            Err(Held::Check(
                Refusal::NotNow(_) | Refusal::IdentityUnavailable(_) | Refusal::IdentityUnconfirmed,
            )) => PayOutcome::Pending { attempt_id },
            // A destination refused, an attempt the claim or the identity no longer backs:
            // "may be paid", for a receipt or the user.
            Err(Held::Check(_)) => PayOutcome::Uncertain { attempt_id },
        }))
    }

    /// The gate before money moves, the same for a first dispatch and a repeat (Paykit spec
    /// 4, 7.2 step 1). In order:
    ///
    /// 1. the app may make an attempt now: no alarm, not left, no restore question, not in
    ///    reconcile mode, no payer pass owed since a restore ([`Paykit::payer_pass_owed`])
    ///    and, for an automatic payment, the clocks within the tolerance (Switch spec 10.1,
    ///    12.4; Paykit spec 7.3);
    /// 2. `claim`, the view of a checked proof or of a claim read, shows the attempt open,
    ///    this app's and current (Paykit spec 7.2, "Only a verified answer lets money move");
    /// 3. the destination passes its checks against its carrier and the payee's
    ///    `identity.json`, for the initiator (Paykit spec 4, 7.2; Switch spec 4.3);
    /// 4. the own `identity.json` is confirmed in the last 60 s, read again first with
    ///    `refresh`, judged on the clock after the read (Paykit spec 4);
    /// 5. that copy lists this app, not removed, with the certificate it holds counting now
    ///    (Paykit spec 4, Switch spec 3.2);
    /// 6. that copy names the generation the attempt was written at, and the service whose
    ///    key signed the attempt's transition proof (Paykit spec 4, "Fresh before paying";
    ///    Switch spec 12.5, 12.6).
    ///
    /// `Err` inside holds the first refusal, as [`Held::OwnIdentity`] for 4, and
    /// [`Refusal::NotCurrent`] for 5 and 6.
    pub(crate) async fn pay_gate(
        &mut self,
        entry: &JournalEntry,
        carrier: &Carrier,
        claim: &ClaimView,
        by: Initiator,
        unconfirmed: Option<UserConfirmed>,
        refresh: bool,
    ) -> Result<std::result::Result<PeerIdentity, Held>> {
        if let Err(refusal) = self.app.may_attempt(by == Initiator::Automatic) {
            return Ok(Err(Held::Check(Refusal::NotNow(refusal.to_string()))));
        }
        if self.payer_pass_owed().await? {
            return Ok(Err(Held::Check(crate::hold::payer_pass_refusal())));
        }
        let attempt = entry.attempt()?.to_bytes();
        let me = self.app_id();
        let open = claim.status == ClaimStatus::Attempting
            && claim.owner == Some(me)
            && claim.attempt_id == Some(attempt)
            && claim
                .attempt(&attempt)
                .is_some_and(|a| a.owner == me && a.outcome == Outcome::Open);
        if !open {
            return Ok(Err(Held::Check(Refusal::MayBePaid {
                reason: "Paykit spec 7.2: the claim doesn't show this app's attempt open".into(),
                doubt: None,
            })));
        }
        let request_hash = entry.request_hash()?;
        let checked = self
            .check_destination(
                &entry.payee,
                &entry.destination,
                carrier,
                request_hash.as_ref(),
                by,
                unconfirmed,
            )
            .await?;
        let doc = match checked {
            Ok(doc) => doc,
            Err(refusal) => return Ok(Err(Held::Check(refusal))),
        };
        if let Err(refusal) = self.own_identity_confirmed(refresh).await {
            return Ok(Err(Held::OwnIdentity(refusal)));
        }
        // Nothing time-bound is judged before an await and trusted after it: the own
        // identity.json's read may have taken the destination past its expiry.
        if let Err(refusal) = self.destination_valid(
            &entry.payee,
            &entry.destination,
            carrier,
            request_hash.as_ref(),
            &doc.doc,
        ) {
            return Ok(Err(Held::Check(refusal)));
        }
        if let Err(why) = self.own_identity_authorizes(entry) {
            return Ok(Err(Held::Check(Refusal::NotCurrent(why))));
        }
        Ok(Ok(doc))
    }

    /// Checks 5 and 6 of [`Paykit::pay_gate`], on the own `identity.json` held.
    fn own_identity_authorizes(&self, entry: &JournalEntry) -> std::result::Result<(), String> {
        let doc = self.app.own_identity();
        let me = self.app_id();
        let cert = self.app.cert().hash();
        if doc.lists_removed(&me)
            || doc.listed_cert(&me).is_none()
            || !doc.counts_at(&me, &cert, self.now())
        {
            return Err(
                "Paykit spec 4: the own identity.json doesn't list this app with its certificate"
                    .into(),
            );
        }
        let Some(signed) = &entry.attempt_proof else {
            return Err("Paykit spec 4: the attempt's proof isn't kept".into());
        };
        let generation = TransitionProof::peek(signed)
            .map_err(|_| "Paykit spec 4: the attempt's proof doesn't read".to_owned())?
            .envelope()
            .generation;
        if generation != doc.generation() {
            return Err(
                "Switch spec 12.5: the identity names another generation than the attempt's".into(),
            );
        }
        let verified = doc
            .service_key_of(generation)
            .is_some_and(|key| TransitionProof::verify(signed, &key, &self.identity()).is_ok());
        if !verified {
            return Err(
                "Paykit spec 4: the identity names another service than the one that signed the \
                 attempt"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Whether `error` says the own switch doesn't answer now: the request or its answer was
/// lost, the switch answered `503` (`unavailable`, or `staged` while its account is set
/// up), or an answer with a `5xx` status came without the service's signature, as from a
/// proxy in front of it; or the account may have been erased, and the app's wait before it
/// takes it as gone goes on (Switch spec 12.7).
pub(crate) fn switch_unavailable(error: &AppError) -> bool {
    match error.unkept() {
        AppError::Transport(_) | AppError::NotAnsweringAsAccount { .. } => true,
        AppError::Switch(pubky_switch_client::error::ClientError::Transport(_)) => true,
        AppError::Switch(pubky_switch_client::error::ClientError::BadServiceSignature {
            status,
        }) => *status >= 500,
        other => matches!(
            other.code(),
            Some(ErrorCode::Unavailable | ErrorCode::Staged)
        ),
    }
}

/// From when a journal entry at `stage` may be pruned ([`Paykit::prune_journal`]): a
/// `Finished` or `Abandoned` one from its `deadline` plus [`RECONCILIATION_MS`], or, without
/// one, from [`MAX_CLAIM_AGE_MS`] after it was journaled. `None` for one a later reconcile
/// needs: every other stage, and one whose payment method may still show it paid or that
/// waits for a late receipt (`asked_about`).
pub(crate) fn prune_bound(
    stage: Stage,
    asked_about: bool,
    deadline: Option<u64>,
    journaled_at_ms: u64,
) -> Option<u64> {
    if !matches!(stage, Stage::Finished | Stage::Abandoned) || asked_about {
        return None;
    }
    Some(match deadline {
        Some(deadline) => deadline.saturating_add(RECONCILIATION_MS),
        None => journaled_at_ms.saturating_add(MAX_CLAIM_AGE_MS),
    })
}

#[cfg(test)]
mod tests {
    use pubky_switch_client::error::{ClientError, TransportError};

    use super::*;

    fn lost() -> AppError {
        AppError::Transport(TransportError::Connect("down".into()))
    }

    #[test]
    fn test_prune_bound_passed_before_the_service_time() {
        assert!(super::bound_passed(10, Some(11), 11));
        assert!(!super::bound_passed(10, Some(10), 11));
    }

    #[test]
    fn test_prune_bound_not_passed_by_a_far_service_time_or_without_one() {
        let day = pubky_switch_core::records::DAY_MS;
        // A service time far ahead of the device clock passes only up to a day after it.
        assert!(!super::bound_passed(5 * day, Some(100 * day), day));
        assert!(super::bound_passed(5 * day, Some(100 * day), 5 * day));
        assert!(!super::bound_passed(0, None, 100 * day));
    }

    #[test]
    fn test_prune_bound_only_for_a_finished_or_abandoned_entry() {
        let kept = [
            Stage::Recorded,
            Stage::Attempted,
            Stage::Paying,
            Stage::RailPaid,
            Stage::RailFailed,
            Stage::AcceptanceOwed,
        ];
        for stage in kept {
            assert_eq!(prune_bound(stage, false, Some(5), 1), None, "{stage:?}");
            assert_eq!(prune_bound(stage, false, None, 1), None, "{stage:?}");
        }
        for stage in [Stage::Finished, Stage::Abandoned] {
            assert_eq!(
                prune_bound(stage, false, Some(5), 1),
                Some(5 + RECONCILIATION_MS)
            );
            assert_eq!(
                prune_bound(stage, false, None, 1),
                Some(1 + MAX_CLAIM_AGE_MS)
            );
            // Still asked about, or waiting for a late receipt: kept.
            assert_eq!(prune_bound(stage, true, Some(5), 1), None, "{stage:?}");
        }
    }

    #[test]
    fn test_switch_unavailable_through_a_kept_batch_and_the_erasure_wait() {
        assert!(switch_unavailable(&lost()));
        assert!(switch_unavailable(&AppError::Kept {
            batch_id: [1; 16],
            error: Box::new(lost()),
        }));
        assert!(switch_unavailable(&AppError::NotAnsweringAsAccount {
            since_ms: 1
        }));
        assert!(switch_unavailable(&AppError::Switch(
            ClientError::BadServiceSignature { status: 503 }
        )));
    }

    #[test]
    fn test_switch_unavailable_not_for_an_answer_it_signed() {
        assert!(!switch_unavailable(&AppError::Switch(
            ClientError::BadServiceSignature { status: 404 }
        )));
        assert!(!switch_unavailable(&AppError::Kept {
            batch_id: [1; 16],
            error: Box::new(AppError::Switch(ClientError::BadServiceSignature {
                status: 404
            })),
        }));
    }
}
