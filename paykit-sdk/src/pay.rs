//! Paying (Paykit spec 7.1, 7.2, 8.1, 8.4, 8.5): the paying order, with the payment journal.
//!
//! 1. **Attempt.** The transfer is checked first: the payment method must transfer exactly
//!    the amount paid, which the payer computes before paying ([`crate::conversion`]): the
//!    request's amount, or, to an endpoint a rate of the request's conversion terms prices,
//!    the exact amount authorized for the payment ([`PricedPayment`]). An automatic
//!    rate-priced payment requires an eligible own-policy period. Then, before any write, the own `identity.json` (read again, and confirmed within
//!    the last 60 s) and the payee's; the payment is journaled; then the `attempt` is
//!    written on the obligation's claim, conditioned on the version of the request record
//!    the app read and carrying the request's deadline. A payment without a request writes
//!    its `payer_activity` record in the same batch, and a single-use endpoint its
//!    `payment_lists` mark. The client checks the attempt's proof: only a verified answer
//!    showing this app's own open attempt lets money move.
//! 2. **Check, then pay.** The own `identity.json` is still confirmed, the destination checked
//!    ([`crate::destination`]), the method's transfer still the amount paid, and the rates a
//!    priced payment was confirmed by still usable; a failure writes `failed` instead of
//!    paying. The start of the payment is journaled, then the rail pays with the attempt ID
//!    as its idempotency key.
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

mod offline;

use std::collections::HashMap;

use paykit_lib::bodies::{
    PaymentProof, PaymentRequest, PaymentRequestAcceptance, PrivatePaymentListRequest,
    RequestDestination,
};
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{
    ActivityRecord, ActivitySide, DestinationSource, PaymentListRecord, StoredMessage, UsedEndpoint,
};
use paykit_lib::{
    AttemptId, Body, ListVersion, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    Rate,
};
use pubky_dispatch_client::app::{
    claim_conflict, AppError, ClaimAttempt, ClaimOp, Initiator, KeptBatch, KeptBatchStatus,
    LocalWrite, ObligationDeadline, OpBatch, PeerIdentity, RecordContent, Refusal as ClientRefusal,
    SharedRecord, SignedMessage, SwitchCopy, UserConfirmed, REVALIDATION_MS,
};
use pubky_dispatch_core::chain::TransitionProof;
use pubky_dispatch_core::claims::{
    Basis, ClaimStatus, ClaimView, Outcome, MAX_CLAIM_AGE_MS, RECONCILIATION_MS,
};
use pubky_dispatch_core::http::ErrorCode;
use pubky_dispatch_core::ops::{Condition, OpKind, RecordVersion};
use pubky_dispatch_core::records::{CLOCK_TOLERANCE_MS, DAY_MS};

use crate::activity::{self, ActivityTarget, Role};
use crate::app_records::PublicAppFailure;
use crate::conversion::{self, Priced, PricedPayment, RateSource};
use crate::destination::{payee_copy_usable, Carrier, Need};
use crate::error::{PaykitError, Refusal, Result};
use crate::journal::{
    AttemptFacts, Destination, JournalEntry, KeptAmount, KeptPrice, PaymentView, Stage,
    TransferBound,
};
use crate::notices::NoticeKind;
use crate::paykit::Paykit;
use crate::rail::{
    CandidateId, PaymentEndpointCandidate, RailError, RailOutcome, RailPaid, RailPayment, Transfer,
};
use crate::records::{list_target, KeptMessage};
use crate::requests::{
    cancelled, payment_ended, payment_history_lost, rejected, request_of, unreadable_request,
    Fresh, RequestRef,
};
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
    /// A Billing Period with incomplete pre-loss payment history stays held.
    pub may_be_paid_confirmed: Option<PaymentDoubt>,
    /// The transfer the user was shown and chose to pay anyway, as a refusal for another
    /// amount gave it ([`Refusal::Transfer`]; Paykit spec 7.2
    /// step 1). It lets only that destination, with that same transfer, be paid: one chosen
    /// or quoted differently since is refused again with what it would transfer now.
    ///
    /// Consent is needed only for what the user didn't ask for. A destination whose
    /// transfer passes the check, the amount asked for, is paid
    /// without it, on a call carrying a confirmation too: the user asked for that payment
    /// when they called pay, as in v0.1, where the wallet paid the target it resolved. The
    /// confirmed quote is then not used.
    pub transfer_confirmed: Option<TransferQuote>,
    /// The exact quote authorized by the user or by a bounded payment policy the user
    /// approved (Paykit spec 7.2 step 1, 8.2). Return the quote from
    /// [`Refusal::PricedByRate`] for this request and occurrence. The SDK checks its
    /// destination, signed source, rate and resulting amount again before paying.
    pub rate_authorized: Option<PricedPayment>,
    /// Inspect the selected endpoint without opening an attempt or asking for a list.
    pub(crate) quote_only: bool,
    /// The signed rate details to use for a Billing Period, identified by their message ID.
    /// By default the latest usable details that price the chosen endpoint are used.
    pub period_details: Option<String>,
    /// The user has seen that the payee's Dispatch couldn't confirm the payee's apps, and pays
    /// anyway ([`Refusal::IdentityUnconfirmed`]; Dispatch spec 4.3, Paykit spec 7.2).
    pub unconfirmed_identity_confirmed: Option<UserConfirmed>,
    /// The user has seen that the payee may have cancelled this request since the backup the
    /// app was restored from, or a `missed` item, and pays it anyway
    /// ([`Refusal::HeldAfterRestore`]; Paykit spec 7.2, "What a restore loses"). It ends the
    /// request's hold; it never overrides a cancellation the app holds. Return the exact
    /// hold shown: another restore or missed item requires a new confirmation.
    pub possibly_cancelled_confirmed: Option<crate::HeldRequest>,
    /// The payment of a request from a blocked payee the user was shown and chose to make,
    /// as a [`Refusal::Blocked`] gave it (Paykit spec 8.1): only that same payment is made.
    pub blocked_confirmed: Option<BlockedPayment>,
    /// The user has confirmed paying a Billing Period before it starts (Paykit spec 8.2,
    /// "Not before its start"): without it a period that hasn't started is
    /// [`Refusal::PeriodNotStarted`].
    pub early_payment_confirmed: Option<UserConfirmed>,
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
    /// amount gave it ([`Refusal::Transfer`]; Paykit spec 7.2
    /// step 1). Only that destination, with that same transfer, is paid.
    #[must_use]
    pub fn with_transfer_confirmed(mut self, quote: TransferQuote) -> Self {
        self.transfer_confirmed = Some(quote);
        self
    }

    /// Authorize the exact quote from [`Refusal::PricedByRate`] for this payment,
    /// after user confirmation or under a bounded policy the user approved.
    #[must_use]
    pub fn with_rate_authorized(mut self, payment: PricedPayment) -> Self {
        self.rate_authorized = Some(payment);
        self
    }

    /// Select the signed details supplying this Billing Period’s destination, rates, or both (Paykit spec 8.2).
    #[must_use]
    pub fn with_period_details(mut self, msg_id: impl Into<String>) -> Self {
        self.period_details = Some(msg_id.into());
        self
    }

    /// An explicit choice, or the still-valid signed details whose amount the user accepted.
    pub(crate) fn selected_period_rate(&self) -> Option<&str> {
        self.period_details
            .as_deref()
            .or_else(|| match &self.rate_authorized.as_ref()?.source {
                RateSource::PeriodDetails { msg_id } => Some(msg_id.as_str()),
                RateSource::Request => None,
            })
    }

    /// The user has seen that the payee's Dispatch couldn't confirm the payee's apps, and pays
    /// anyway ([`Refusal::IdentityUnconfirmed`]; Dispatch spec 4.3, Paykit spec 7.2).
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
    pub fn with_possibly_cancelled_confirmed(mut self, confirmed: crate::HeldRequest) -> Self {
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

    /// The user has confirmed paying a Billing Period before it starts (Paykit spec 8.2,
    /// "Not before its start"): its attempt is applied from the latest service time verified
    /// once the user confirmed, never earlier.
    #[must_use]
    pub fn with_early_payment_confirmed(mut self, confirmed: UserConfirmed) -> Self {
        self.early_payment_confirmed = Some(confirmed);
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
    /// The Billing Period paid, for a recurring request.
    pub period: Option<PeriodRef>,
    /// The amount paid.
    pub amount: PaymentAmount,
}

/// Refuse a payment the user starts to a payee the deny list holds, `blocked`, unless the
/// user `confirmed` `shown`, the same payment (Paykit spec 8.1).
pub(crate) fn refuse_blocked(
    blocked: bool,
    shown: BlockedPayment,
    confirmed: Option<&BlockedPayment>,
) -> Result<()> {
    if blocked && confirmed != Some(&shown) {
        return Err(Refusal::Blocked {
            payment: Box::new(shown),
        }
        .into());
    }
    Ok(())
}

/// Why a payment may already have been made, as a [`Refusal::MayBePaid`] shows it to the
/// user (Paykit spec 7.1, 7.2, 7.3): the obligation, the claim's step read, the attempts that
/// make it doubtful by name, the claim's flags, and whether a provider loss leaves it in
/// doubt. A payment the user then makes anyway carries it back
/// ([`PayOptions::with_may_be_paid_confirmed`]): the SDK reads the claim again and goes on
/// only when the doubt now is this one, field for field. No automatic payment goes over
/// [`PaymentDoubt::before_loss`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PaymentDoubt {
    /// The claim key.
    pub obligation: [u8; 32],
    /// The claim's step read; `None` for no claim.
    pub step: Option<u64>,
    /// This app's attempts a restore left unresolved (Dispatch spec 12.4).
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
    /// The provider loss, when the obligation was payable before it and its claim at the new
    /// service isn't SETTLED: another own app, or this one, may have paid it at the old
    /// service, and nothing of that reaches the new one (Paykit spec 7.3). It names the loss
    /// that decides the doubt now, so a confirmation of a doubt shown under an older loss
    /// is refused again once a newer one decides. The host shows the user that it may
    /// already be paid. For a one-time request the user can pay over the doubt or state
    /// that it wasn't paid before that loss ([`Paykit::confirm_not_paid_before_loss`]).
    /// A Billing Period with incomplete pre-loss payment history stays held.
    pub before_loss: Option<crate::LossBoundary>,
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
            || self.before_loss.is_some()
    }
}

/// What a destination would transfer, as a payment shows it to the user before paying it on
/// their explicit choice (Paykit spec 7.2 step 1): the destination, named by its payee app,
/// Payment Endpoint Identifier and a hash of its payload, never the payload itself; the
/// amount passed to the method; and the whole [`Transfer`] the payment method reported. A
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
    /// The amount passed to the payment method: the request's computed amount, or the
    /// amount chosen for a payment without a request. `transfer.amount` separately names
    /// what the method would actually transfer, if known.
    pub amount: PaymentAmount,
    /// What the method said it would transfer: its amount.
    pub transfer: Transfer,
}

/// The payment outcome or error and any payee apps whose public endpoints the call
/// could not use, with the reason for each (Paykit spec 4).
///
/// Returned by [`Paykit::pay_request`], [`Paykit::pay_under_allowance`], [`Paykit::pay_period`],
/// [`Paykit::pay_period_automatically`], [`Paykit::pay_period_under_allowance`]
/// and [`Paykit::pay_without_request`].
///
/// The failures come with every outcome and every error alike, so the host can tell the user
/// of each failed app, as it must of records that conflict (Dispatch wire 3.2), whatever the
/// payment did. A failure doesn't mean the payment went to another app: an app whose records
/// conflict is listed and can still be paid from the earlier record it holds (module docs of
/// [`crate::app_records`]). A payment resumed or repaired later ([`Paykit::resume_payments`],
/// [`Paykit::repair_activity_record`]) reads no public endpoints and returns its outcome
/// alone.
#[derive(Debug)]
#[non_exhaustive]
#[must_use = "a payment's outcome or error, and the failed apps to tell the user of"]
pub struct PayResult {
    /// The payment's outcome, or the error that stopped it, as a payment call returned
    /// before: `outcome?` gives the outcome.
    pub outcome: Result<PayOutcome>,
    /// The payee's apps whose public endpoints couldn't be used in this call's read, in
    /// [`PaymentMode::Public`]; empty for a call that read no public endpoints. A payment
    /// refused with [`Refusal::NoDestination`] names them in its text as well.
    pub public_failures: Vec<PublicAppFailure>,
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
    /// Paid by the method, but the outcome's batch didn't go through (the Dispatch didn't
    /// answer, or the write failed otherwise). The journal keeps the proof and the attempt
    /// stays open, so nothing pays again; a resume ([`Paykit::resume_payments`]) writes the
    /// outcome and sends the proof (Paykit spec 7.2 steps 3 and 4).
    #[non_exhaustive]
    PaidOutcomeUnwritten {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// Paid by the method. The journal keeps the proof and the attempt stays open while
    /// its activity update waits (Paykit spec 7.2 step 3). For an unreadable value, resume
    /// once it reads or use [`Paykit::repair_activity_record`]. When recovery shows another
    /// signed write at the held version, resume once the client accepts a matching write or
    /// a successor.
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
    /// Over, with nothing of this call's to tell: the attempt was resolved by the user or by
    /// another app, or it finished with nothing of this call's to report. The claim and
    /// [`Paykit::payments`] say which. It is not a statement that no payment was made.
    #[non_exhaustive]
    Resolved {
        /// The attempt.
        attempt_id: AttemptId,
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
            | Self::RecordDivergent { attempt_id, .. }
            | Self::Resolved { attempt_id, .. } => *attempt_id,
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
    /// The Dispatch client holds attempts back (check 1): an alarm, an app that left, a
    /// recovery notice, a restore question, reconcile mode or the clocks disagreeing.
    Client(ClientRefusal),
    /// The own `identity.json` isn't confirmed in the last 60 s (check 4).
    OwnIdentity(Refusal),
    /// Any other check.
    Check(Refusal),
}

/// A `payment_lists` record a payment selected from, with the version of the write whose
/// content it is ([`Paykit::lists_read_of`]).
pub(crate) type ListRead = (PaymentListRecord, RecordVersion);

/// The identifiers that both the request and any funding Allowance permit.
pub(crate) fn accepted_request_endpoints(
    request: &PaymentRequest,
    allowance_endpoints: Option<&[PaymentEndpointIdentifier]>,
) -> Option<Vec<PaymentEndpointIdentifier>> {
    match &request.accepted_payment_endpoint_identifiers {
        Some(ids) => Some(
            ids.iter()
                .filter(|id| allowance_endpoints.is_none_or(|allowed| allowed.contains(id)))
                .cloned()
                .collect(),
        ),
        None => allowance_endpoints.map(<[_]>::to_vec),
    }
}

/// What one attempt needs beyond its journal entry.
pub(crate) struct Plan {
    pub(crate) entry: JournalEntry,
    pub(crate) record: ActivityTarget,
    pub(crate) carrier: Carrier,
    pub(crate) step: u64,
    /// For a list endpoint, the `payment_lists` record of its list and the version selected
    /// from: the attempt is conditioned on it, and its batch writes a single-use mark.
    pub(crate) reserve: Option<ListRead>,
    /// The record a payment without a request creates in the attempt's batch.
    pub(crate) create: Option<ActivityRecord>,
    /// Condition the attempt on the version of the request record read.
    pub(crate) on_record: bool,
    pub(crate) allowance: Option<crate::executor::AllowanceReservation>,
    /// Who starts the payment: the user, or an executor (Dispatch spec 4.3).
    pub(crate) by: Initiator,
    /// The user's confirmation of a payee identity its Dispatch couldn't confirm.
    pub(crate) unconfirmed: Option<UserConfirmed>,
    /// The blocked payment the user confirmed, checked against the selected payment.
    pub(crate) blocked_confirmed: Option<BlockedPayment>,
    /// The restore or `missed` hold the user's confirmation ends, in the attempt's batch: a
    /// call refused before it keeps the hold.
    pub(crate) end_hold: Option<crate::hold::HoldEnd>,
    /// More conditions of the attempt, such as a Billing Period's `server_time_in`.
    pub(crate) conditions: Vec<Condition>,
    /// Record writes of the attempt's batch, such as a Billing Period's `attempted_periods`
    /// and `manual_only` (Paykit spec 8.2, 8.3).
    pub(crate) record_writes: Vec<(Vec<Condition>, OpKind)>,
}

/// A Payment Endpoint a payment may take: its identifier and payload, handed to the payment
/// method as a candidate ([`crate::Rail::select_endpoints`]), and what paying it needs: the
/// destination journaled, its carrier, and, for a list endpoint, the `payment_lists` record
/// and the version the attempt is conditioned on, which its batch marks for a single-use one.
pub(crate) struct Choice {
    endpoint_id: PaymentEndpointIdentifier,
    payload: PaymentEndpointPayload,
    destination: Destination,
    carrier: Carrier,
    reserve: Option<ListRead>,
}

impl Choice {
    pub(crate) fn new(
        endpoint_id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        destination: Destination,
        carrier: Carrier,
        reserve: Option<ListRead>,
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

/// A selected destination with its call argument, exact transfer evidence and kept price.
pub(crate) struct Chosen {
    pub(crate) destination: Destination,
    pub(crate) carrier: Carrier,
    pub(crate) reserve: Option<ListRead>,
    pub(crate) bound: TransferBound,
    pub(crate) payment_amount: PaymentAmount,
    pub(crate) paid: Option<PaymentAmount>,
    pub(crate) price: Option<KeptPrice>,
}

/// How a payment's amount to each endpoint is computed (Paykit spec 7.2 step 1,
/// [`conversion::price`]): the amount asked, the request paid, and, under conversion terms,
/// the rates and where they come from.
pub(crate) struct Pricing<'a> {
    /// The amount asked: the request's, or the payment without a request's.
    requested: &'a PaymentAmount,
    /// The request paid; `None` for a payment without a request, which pays its own amount.
    request: Option<RequestRef>,
    period: Option<PeriodRef>,
    rates: Rates<'a>,
    by: Initiator,
    under_allowance: bool,
}

/// The signed sources of prices for the endpoints of a payment (Paykit wire 4.4, 4.9).
enum Rates<'a> {
    None,
    Fixed(&'a [Rate]),
    /// Usable details of the selected occurrence, latest first.
    Period(Vec<&'a StoredMessage>),
}

impl<'a> Pricing<'a> {
    pub(crate) fn unrequested(amount: &'a PaymentAmount) -> Self {
        Self {
            requested: amount,
            request: None,
            period: None,
            rates: Rates::None,
            by: Initiator::User,
            under_allowance: false,
        }
    }

    pub(crate) fn of_request(
        requested: &'a PaymentAmount,
        request: RequestRef,
        period: Option<PeriodRef>,
        conversion: Option<&'a paykit_lib::bodies::Conversion>,
        by: Initiator,
    ) -> Self {
        let rates = match conversion::fixed_rates(conversion) {
            Some(rates) => Rates::Fixed(rates),
            None if conversion.is_some() => Rates::Period(Vec::new()),
            None => Rates::None,
        };
        Self {
            requested,
            request: Some(request),
            period,
            rates,
            by,
            under_allowance: false,
        }
    }

    pub(crate) fn under_allowance(mut self) -> Self {
        self.under_allowance = true;
        self
    }

    pub(crate) fn with_period_rates(mut self, details: Vec<&'a StoredMessage>) -> Self {
        self.rates = Rates::Period(details);
        self
    }

    /// Price this endpoint and retain the exact signed details supplying its rate.
    pub(crate) fn price(
        &self,
        rail: &dyn crate::Rail,
        endpoint: &PaymentEndpointIdentifier,
        usable: impl Fn(&StoredMessage) -> bool,
    ) -> (Priced, Option<&'a StoredMessage>) {
        let (rates, source) = match &self.rates {
            Rates::None => (None, None),
            Rates::Fixed(rates) => (Some(*rates), None),
            Rates::Period(details) => {
                let selected = details
                    .iter()
                    .copied()
                    .filter(|signed| usable(signed))
                    .find_map(|signed| {
                        let Body::PaymentPeriodDetails(body) = signed.body() else {
                            return None;
                        };
                        let rates = body.rates.as_deref()?;
                        matches!(paykit_lib::rate_for_endpoint(rates, endpoint), Ok(Some(_)))
                            .then_some((rates, signed))
                    });
                match selected {
                    Some((rates, signed)) => (Some(rates), Some(signed)),
                    None => (None, None),
                }
            }
        };
        let price = conversion::price(rail, self.requested, rates, endpoint);
        if matches!(self.rates, Rates::Period(_))
            && source.is_none()
            && matches!(price, Priced::ByHand(_))
        {
            return (
                Priced::Unusable("Paykit spec 8.2: no period details price this endpoint"),
                None,
            );
        }
        (price, source)
    }
}

/// What a payment pays to one destination, as [`Paykit::price_choice`] judges it.
enum Taken {
    /// The amount paid, and the price its user confirmed for one priced by a rate.
    Pay(PaymentAmount, Option<KeptPrice>),
    /// The amount handed to the method for a payment made only by hand
    /// ([`Priced::ByHand`]): paid only on its user's confirmation of what the method would
    /// transfer for it ([`Paykit::check_transfer`]).
    ByHand(PaymentAmount),
    /// Unusable for the payment, and why.
    Unusable(&'static str),
}

/// The payment method's answer, as [`Paykit::rail_order`] checked it: the places of the
/// candidates it pays, in its order, and the first error of one it couldn't judge, by the
/// order handed in.
struct Ordered {
    payable: Vec<usize>,
    error: Option<RailError>,
}

/// Why the transfer check refuses `transfer` for `amount` (Paykit spec 7.2 step 1): another
/// amount or asset, or one the method can't tell. Paykit makes no check of the fee: it is
/// the payment method's. `None` when it passes.
fn transfer_refused(transfer: &Transfer, amount: &PaymentAmount) -> Option<&'static str> {
    (transfer.amount.as_ref() != Some(amount)).then_some(
        "the destination transfers another amount or asset, or one the method can't tell",
    )
}

/// Whether `quote`, the transfer the method reports now, is the one the user confirmed,
/// `confirmed` (Paykit spec 7.2 step 1): the same destination, amount and transfer amount.
/// Amounts compare as values, whatever their spelling (Paykit wire 1.2).
fn confirms(confirmed: &TransferQuote, quote: &TransferQuote) -> bool {
    confirmed == quote
}

/// A fixed rate lasts as long as its request. A period rate needs both clocks.
fn rates_usable(
    source: &RateSource,
    selected: Option<&StoredMessage>,
    device: u64,
    service: Option<u64>,
) -> bool {
    match source {
        RateSource::Request => true,
        RateSource::PeriodDetails { msg_id } => selected.is_some_and(|details| {
            &details.message().msg_id == msg_id
                && crate::periods::rates_current(details, device, service)
        }),
    }
}

/// The amount a payment of a request that no rate prices pays now, `priced` without rates,
/// at the final payment check (Paykit spec 7.2 step 1), and whether it is made only by hand.
/// A payment made only by hand is never a first handoff of an automatic payment
/// (`automatic_first`): planning refuses it, and this check refuses it again just before
/// the call. `Err` inside holds the refusal of an endpoint unusable now.
///
/// # Errors
///
/// A corrupt store for an amount a rate prices, which no selection without rates gives.
fn unpriced_now(
    priced: Priced,
    automatic_first: bool,
) -> Result<std::result::Result<(PaymentAmount, bool), Refusal>> {
    Ok(match priced {
        Priced::Plain(now) => Ok((now, false)),
        Priced::ByHand(_) if automatic_first => Err(Refusal::UnsupportedEndpoint(
            conversion::BY_HAND_ONLY.into(),
        )),
        Priced::ByHand(now) => Ok((now, true)),
        Priced::ByRate { .. } => return Err(PaykitError::corrupt()),
        Priced::Unusable(why) => Err(Refusal::UnsupportedEndpoint(format!("{why}, now"))),
    })
}

/// The refusal of a destination whose endpoints the payment method pays none of.
fn unsupported(what: &str) -> PaykitError {
    Refusal::UnsupportedEndpoint(format!(
        "Paykit spec 7.2: the payment method pays none of {what}"
    ))
    .into()
}

impl Paykit {
    /// Refuse a payment at its start for what no read of the payment clears: an alarm, an
    /// app that left or a restore question (Dispatch spec 3.6, 12.3, 12.4), before the reads a
    /// payment makes first. A recovery notice, which those reads may clear, reconcile mode
    /// and the clocks are judged with the rest of `may_attempt` just before the attempt.
    pub(crate) fn refuse_held_back(&self) -> Result<()> {
        match self.app.may_attempt(false) {
            Err(refusal) if refused_for_good(&refusal) => Err(held_back(refusal)),
            _ => Ok(()),
        }
    }
}

/// Whether the Dispatch client refuses attempts until the user or a recovery acts: an alarm, an
/// app that left or a restore question. A recovery notice may clear on the next own request,
/// and its `NotNow` on a wait.
fn refused_for_good(refusal: &ClientRefusal) -> bool {
    !matches!(
        refusal,
        ClientRefusal::NotNow(_) | ClientRefusal::RecoveryRequired { .. }
    )
}

/// What holds a request from automatic payment (Paykit spec 7.2, 8.1; Paykit wire 5.6).
#[derive(Clone, Copy, Debug)]
pub(crate) enum AutomaticHold {
    /// The deny list holds the payee.
    Blocked,
    /// The block floor holds the request, as this app holds the floor.
    Floor(crate::floor::SharedFloor),
    /// A restore's or a `missed` item's hold the user hasn't confirmed.
    Restore,
    /// A pause that blocks automatic payment.
    Paused,
}

impl AutomaticHold {
    /// The user answers a block, a floor known to hold the request, a hold or a pause: by an
    /// unblock, a confirmation or a payment by hand. A floor not known waits for a read of
    /// it.
    pub(crate) fn refusal(self) -> Refusal {
        match self {
            // A floor not read yet: a later read tells it. One that doesn't read, or is gone,
            // is no wait: the request is paid by hand, as under any hold below.
            Self::Floor(crate::floor::SharedFloor::Unknown(crate::floor::FloorUnknown::Unread)) => {
                Refusal::NotNow(crate::NotNowReason::ReadAgain(
                    "this request is held from automatic payment while the block floor isn't \
                     known",
                ))
            }
            // Held for good (Paykit wire 5.6, Divergent floor): the refusal that names it.
            Self::Floor(crate::floor::SharedFloor::Divergent) => Refusal::FloorHeld,
            Self::Blocked
            | Self::Floor(
                crate::floor::SharedFloor::Known(_)
                | crate::floor::SharedFloor::Unknown(
                    crate::floor::FloorUnknown::Unreadable | crate::floor::FloorUnknown::Gone,
                ),
            )
            | Self::Restore
            | Self::Paused => Refusal::HeldForUser(crate::HeldForUserReason::HeldFromAutomatic),
        }
    }
}

/// What a first handoff does with what held it at the pay gate.
enum FirstHold {
    /// Returned as it is: nothing is paid and the attempt stays `Attempted`, known
    /// unsubmitted, for a resume to write it `failed` once the client lets it write.
    Return(PaykitError),
    /// The attempt is written `failed` for this refusal ([`Paykit::fail_attempt`]).
    Fail(Refusal),
}

fn first_handoff_hold(held: Held) -> FirstHold {
    match held {
        // Writes still go on in reconcile mode and with the clocks apart: the attempt is
        // closed, so a later payment of the obligation isn't held behind it.
        Held::Client(refusal @ ClientRefusal::NotNow(_)) => FirstHold::Fail(Refusal::HeldForUser(
            crate::HeldForUserReason::SwitchClient(refusal.to_string()),
        )),
        Held::Client(refusal) => FirstHold::Return(held_back(refusal)),
        Held::Check(refusal @ Refusal::NotCurrent(_)) => FirstHold::Return(refusal.into()),
        Held::OwnIdentity(refusal) | Held::Check(refusal) => FirstHold::Fail(refusal),
    }
}

/// The outcome of a repeat the pay gate held, its first handoff made: never paid now.
fn repeat_hold(held: Held, attempt_id: AttemptId) -> Result<PayOutcome> {
    match held {
        // No payment of this app goes on without its own identity.json confirmed.
        Held::OwnIdentity(refusal) => Err(refusal.into()),
        // What may pass later: reconcile mode or the clocks, the payee's identity.json not
        // confirmed yet, or the payer pass a reconcile owes since a restore, which a repeat
        // waits for (Paykit spec 7.3).
        Held::Client(ClientRefusal::NotNow(_))
        | Held::Check(
            Refusal::NotNow(_)
            | Refusal::RecoveryNeeded(crate::RecoveryReason::PayerPassOwed)
            | Refusal::IdentityUnavailable(_)
            | Refusal::IdentityUnconfirmed,
        ) => Ok(PayOutcome::Pending { attempt_id }),
        // An alarm, an app that left, a recovery notice or a restore question: returned, so
        // the caller recovers rather than waits.
        Held::Client(refusal) => Err(held_back(refusal)),
        // A destination refused, an attempt the claim or the identity no longer backs: "may
        // be paid", for a receipt or the user.
        Held::Check(_) => Ok(PayOutcome::Uncertain { attempt_id }),
    }
}

/// Whether `error` stops the wallet as a whole: an alarm, the local store failing, the
/// Dispatch client's word that the app stopped, left or is gone, holds a recovery notice, or
/// has a restore question open, or the Dispatch's answer that the app is no longer current, that
/// its certificate expired, or that the account awaits a recovery (Dispatch spec 3.6, 8.2, 12.2,
/// 12.3, 12.4, Dispatch wire 7.6).
/// A resume ends at it at once and returns it: nothing more is written, looked up or
/// pruned.
pub(crate) fn stops_resume(error: &PaykitError) -> bool {
    use pubky_dispatch_core::http::ErrorCode as E;
    let PaykitError::Client(client) = error else {
        return false;
    };
    if matches!(
        error.code(),
        Some(E::NotCurrent | E::CertExpired | E::RecoveryRequired)
    ) {
        return true;
    }
    matches!(
        client.unkept(),
        AppError::Alarm(_)
            | AppError::Store(_)
            | AppError::Protocol(_)
            | AppError::Refused(
                ClientRefusal::Stopped
                    | ClientRefusal::Left
                    | ClientRefusal::Gone
                    | ClientRefusal::RecoveryRequired { .. }
                    | ClientRefusal::RestoreQuestionOpen
            )
    )
}

/// The one rule for a failure a step meets after work it can't take back, which the step
/// otherwise reports as its own outcome, best effort: a wallet-wide stop ([`stops_resume`])
/// is returned, so nothing more is written, looked up or pruned; anything else is the step's
/// own uncertainty, left to the outcome it reports and to a later resume.
pub(crate) fn unless_stop(error: impl Into<PaykitError>) -> Result<()> {
    let error = error.into();
    if stops_resume(&error) {
        return Err(error);
    }
    Ok(())
}

/// [`unless_stop`] for a failure that may come from another identity's Dispatch, such as a
/// lookup of a payee: only a stop of this app's own state counts there, an alarm, a store
/// failure or the Dispatch client's refusal. A service answer in it may be about the other
/// identity's account, never this wallet's.
pub(crate) fn unless_local_stop(error: impl Into<PaykitError>) -> Result<()> {
    let error = error.into();
    if error.code().is_none() && stops_resume(&error) {
        return Err(error);
    }
    Ok(())
}

/// Whether a failure of one entry's step is a wait, which may pass at a later resume with no
/// one acting. A wallet-wide stop is never one ([`stops_resume`]). Of the Dispatch client's
/// failures, [`client_waits`] decides; of this SDK's own: the own Dispatch or `identity.json`
/// unavailable for now (Paykit spec 4, 8.4), a conflict a later resume reads again, and the
/// payment method's history unreachable for now (Paykit spec 7.2).
///
/// The resume leaves such an entry for the next one and reports it as its outcome. Any other
/// failure of one entry, which a later resume would meet again until the user or the host
/// acts, also leaves only that entry, and the call returns it once every entry is visited,
/// so the caller learns the reason rather than seeing the entry pending at every resume.
pub(crate) fn entry_waits(error: &PaykitError) -> bool {
    if stops_resume(error) {
        return false;
    }
    match error {
        PaykitError::Client(client) => client_waits(client),
        PaykitError::Refused(refusal) => refusal_waits(refusal),
        PaykitError::Conflict(_) | PaykitError::Rail(RailError::Unavailable(_)) => true,
        PaykitError::Rail(RailError::Refused(_) | RailError::InvalidSelection(_))
        | PaykitError::SettlementNoticeUnavailable { .. }
        | PaykitError::PublicEndpointChangePending(_)
        | PaykitError::Lib(_)
        | PaykitError::Homeserver(_) => false,
    }
}

/// Whether a refusal of this SDK's own is a wait: the Dispatch or the wallet's own
/// `identity.json` not answering now, and `NotNow`, every reason of which clears with no one
/// acting. Every other refusal waits for the user, the host or the counterparty, or is final.
fn refusal_waits(refusal: &Refusal) -> bool {
    match refusal {
        Refusal::DispatchUnavailable(_) | Refusal::IdentityUnavailable(_) | Refusal::NotNow(_) => {
            true
        }
        Refusal::NotFound
        | Refusal::ScopeMissing(_)
        | Refusal::Label(_)
        | Refusal::Rejected
        | Refusal::HeldAfterRestore { .. }
        | Refusal::Cancelled
        | Refusal::AllowanceEnded
        | Refusal::AllowanceConflicted
        | Refusal::AllowanceConfirmationNeeded { .. }
        | Refusal::CreditHistoryLost
        | Refusal::AlreadyPaid
        | Refusal::BeingHandledByOtherWallet
        | Refusal::MayBePaid { .. }
        | Refusal::LossChanged { .. }
        | Refusal::DeadlinePassed
        | Refusal::Recurring
        | Refusal::MaybeCredited
        | Refusal::PaymentTimeOutside { .. }
        | Refusal::SettlementBucketFull { .. }
        | Refusal::Subscribed
        | Refusal::NotSubscribed
        | Refusal::PeriodNotStarted
        | Refusal::ManualOnly
        | Refusal::NotAutomatic(_)
        | Refusal::AttemptedPeriodsFull
        | Refusal::Destination(_)
        | Refusal::AttemptFailed(_)
        | Refusal::NoDestination(_)
        | Refusal::UnsupportedEndpoint(_)
        | Refusal::WaitingForList { .. }
        | Refusal::ListDeliveryUnknown(_)
        | Refusal::Transfer { .. }
        | Refusal::PricedByRate { .. }
        | Refusal::IdentityUnconfirmed
        | Refusal::PaymentInProgress
        | Refusal::NotCurrent(_)
        | Refusal::HeldForUser(_)
        | Refusal::RecoveryNeeded(_)
        | Refusal::FloorHeld
        | Refusal::Usage(_)
        | Refusal::RecordHeld(_)
        | Refusal::CertificateRefused(_)
        | Refusal::Blocked { .. }
        | Refusal::Divergent { .. } => false,
    }
}

/// Whether a failure met after work a step can't take back (a payment made, an outcome
/// written) is judged by the resume's table rather than taken as best effort: the Dispatch
/// client's, which [`client_waits`] classifies exactly. This SDK's own refusals stay best
/// effort there, as before.
fn judged_at_resume(error: &PaykitError) -> bool {
    matches!(error, PaykitError::Client(_))
}

/// Whether a failure of the pinned Dispatch client is temporary by its own documentation: the
/// same call may pass later with no one acting. Kept and ahead batches are judged by the
/// failure inside them. The waits:
/// - a batch retired before its first handoff, nothing of it applied (`ExpiredUnsent`);
/// - the network, as the transport says ([`transport_waits`]);
/// - the account-erasure check still waiting (`NotAnsweringAsAccount`, Dispatch spec 12.7);
/// - an answer whose signature doesn't verify with a `5xx` status, as from a proxy in front
///   of the service, nothing of it used (Dispatch wire 7.1);
/// - a `429` given before the body was read, final for nothing (Dispatch wire 7.1), and a
///   signed public read whose time is outside the reader's window, which a fresh read
///   replaces;
/// - a Dispatch answer whose Retry column (Dispatch spec 6.4) lets the same request pass as it
///   is, after `Retry-After`, with a new timestamp or after a fresh read;
/// - a write behind batches the client keeps.
///
/// A failure inside a kept or ahead batch (`Kept`, `Ahead`) is judged as the same failure
/// bare: the client keeps a batch after failures that need the host too.
///
/// No wait: a Dispatch answer that needs a key bundle, a new signature or seal, the sender's
/// `identity.json`, another service or a freed quota first, or none passes; an answer that
/// doesn't verify otherwise, or that doesn't read; a head or proof that doesn't verify,
/// which the client handles as a rollback; a refused origin, an answer too large, a request
/// that can't be built; a body over its cap, no pinned
/// key, a client usage error; data that doesn't verify or read; the client's refusal for
/// now (`NotNow`), whose text alone tells reconcile mode, which the user ends, or a display
/// read the host owes (`DisplayReadPending`, which the client turns into one) from the
/// clocks apart; and the client's other refusals. A wallet-wide stop is decided first ([`stops_resume`]): the store, the
/// protocol's store, an alarm, and the client's word that the app stopped, left, is gone,
/// holds a recovery notice (its own `OwnRecoveryRequired` included, which the client turns
/// into one) or has a restore question open.
pub(crate) fn client_waits(error: &AppError) -> bool {
    use pubky_dispatch_client::error::ClientError as C;
    use pubky_dispatch_core::http::Retry;
    // A kept or ahead batch is judged by the failure inside it, by the same table: the
    // client keeps one the host's own transport refused too, which no resend clears.
    match error.unkept() {
        AppError::ExpiredUnsent | AppError::NotAnsweringAsAccount { .. } => true,
        AppError::Transport(transport) | AppError::Switch(C::Transport(transport)) => {
            transport_waits(transport)
        }
        AppError::Switch(dispatch) => match dispatch {
            C::Switch { code, .. } => matches!(
                code.retry(),
                Retry::Yes | Retry::AfterRetryAfter | Retry::WithNewTimestamp | Retry::AfterReread
            ),
            C::BadServiceSignature { status } => *status >= 500,
            // Before the body was read only a `429` passes by itself: a `413` or a `404` comes
            // again for the same bytes.
            C::BodyUnread { code, .. } => code.retry() == Retry::AfterRetryAfter,
            C::StaleAnswer { .. } => true,
            C::DisplayReadPending
            | C::OwnRecoveryRequired { .. }
            | C::Chain(_)
            | C::InvalidResponse(_)
            | C::Core(_)
            | C::Usage(_)
            | C::BodyOverCap { .. }
            | C::Unpinned => false,
            // Open to new variants (`#[non_exhaustive]`): every variant of the pinned client is
            // named above, and one a later pin adds is no wait until it is classified here.
            _ => false,
        },
        AppError::Refused(ClientRefusal::KeptBatchesAhead(_)) => true,
        AppError::Refused(_)
        | AppError::Store(_)
        | AppError::Protocol(_)
        | AppError::InvalidData(_)
        | AppError::Alarm(_) => false,
        // `unkept` took the failure out of either: none is left here.
        AppError::Kept { .. } | AppError::Ahead { .. } => false,
        // The client's error is open to new variants (`#[non_exhaustive]`), so the compiler
        // asks for this arm: every variant of the pinned client is named above, and one a
        // later pin adds is no wait until it is classified here.
        _ => false,
    }
}

/// Whether a transport failure is the network's, which the client sends again with the same
/// bytes: no connection, a timeout, or another failure of the transport; and a signed header that can't be read,
/// which the client takes as an answer whose signature doesn't verify, under a `5xx` status.
/// No first handoff at all, which a host transport without that facility gives every time,
/// a guarded first send past its deadline, which is never released,
/// an origin the client may not connect to, an answer larger than the transport takes and a
/// request that can't be built are no waits.
fn transport_waits(error: &pubky_dispatch_client::error::TransportError) -> bool {
    use pubky_dispatch_client::error::TransportError as T;
    match error {
        T::Connect(_) | T::Timeout | T::Other(_) => true,
        // Taken as an answer whose signature doesn't verify: a wait under a `5xx` status, as
        // [`client_waits`] judges one.
        T::UnreadableSignedHeader { status } => *status >= 500,
        // A host transport without a first-handoff facility gives it every time.
        // A guarded first send past its deadline is never released.
        T::FirstSendUnavailable
        | T::FirstSendExpired
        | T::Refused(_)
        | T::TooLarge(_)
        | T::InvalidRequest(_) => false,
        // Open to new variants (`#[non_exhaustive]`): every variant of the pinned client is
        // named above, and one a later pin adds is no wait until it is classified here.
        _ => false,
    }
}

/// The reason of a failed outcome written after the payment method reported a failure.
const METHOD_FAILED: &str = "the payment method reported a failure";
/// The reason of a failed outcome written for an attempt no payment started for.
const NO_PAYMENT_STARTED: &str = "Paykit spec 7.2: no payment started";

/// The reason of a failed outcome for an attempt its user resolved as failed.
const USER_FAILED: &str = "the user resolved it as failed";

/// What a resume gathered at the entries' turns that the journal doesn't hold when the call
/// ends ([`resume_outcomes`]): what a step told of an entry it left open that says more than
/// its stage does, that the method's history can't tell, that the app isn't current, or
/// that the payer's record holds the outcome back.
#[derive(Default)]
struct Turns {
    open: HashMap<AttemptId, PayOutcome>,
}

/// What a failure kept at a resume failed at, so the call returns it only while that work
/// is still to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    /// An entry's own step.
    Step,
    /// Placing an acceptance owed.
    Acceptance,
    /// The recheck's lookup of a failure the method reported.
    Recheck,
}

/// A failure a resume met short of a wallet-wide stop, judged once the call's work is done.
struct Pending {
    error: PaykitError,
    /// The attempt and its claim, where the failure is one entry's.
    attempt: Option<(AttemptId, [u8; 32])>,
    work: Work,
}

impl Turns {
    /// Keep what a step told, where it says more of an open entry than its stage does.
    fn tell(&mut self, outcome: PayOutcome) {
        match outcome {
            PayOutcome::Uncertain { .. }
            | PayOutcome::PaidUnreported { .. }
            | PayOutcome::RecordHeld { .. }
            | PayOutcome::RecordDivergent { .. } => {
                self.open.insert(outcome.attempt_id(), outcome);
            }
            // What the journal holds when the call ends says these: nothing to keep.
            PayOutcome::Paid { .. }
            | PayOutcome::Failed { .. }
            | PayOutcome::Pending { .. }
            | PayOutcome::PaidOutcomeUnwritten { .. }
            | PayOutcome::Resolved { .. } => {}
        }
    }
}

/// What a resume's result reads of one journal entry ([`resume_outcomes`]).
#[derive(Clone, Debug, PartialEq, Eq)]
struct EntryFacts {
    attempt_id: AttemptId,
    stage: Stage,
    /// The method's proof is held.
    proof: bool,
    proof_msg_id: Option<String>,
    /// The method said the payment failed ([`AttemptFacts::method_failed`]).
    method_failed: bool,
    /// The recheck still asks the method's history about it.
    ask_again: bool,
    failed_before_call: bool,
    user_resolved: Option<crate::journal::UserResolution>,
    /// A call returned its outcome once it was over ([`AttemptFacts::told`]).
    told: bool,
    /// That outcome was paid ([`AttemptFacts::told_paid`]).
    told_paid: bool,
}

impl EntryFacts {
    /// The facts of every entry that names its attempt, in the journal's order, each read
    /// through the method's word.
    fn of(journal: &[(JournalEntry, AttemptFacts)]) -> Vec<Self> {
        journal
            .iter()
            .filter_map(|(entry, facts)| {
                Some(Self {
                    attempt_id: entry.attempt().ok()?,
                    stage: entry.stage,
                    proof: entry.rail_proof.is_some(),
                    proof_msg_id: entry.proof_msg_id.clone(),
                    method_failed: facts.method_failed || entry.failed_by_method,
                    ask_again: entry.failed_by_method,
                    failed_before_call: entry.failed_before_call,
                    user_resolved: entry.user_resolved,
                    told: facts.told,
                    told_paid: facts.told_paid,
                })
            })
            .collect()
    }

    /// Whether a call returned the outcome the entry stands at now: paid, with the method's
    /// proof, or the one it had without a proof.
    fn told_as_it_stands(&self) -> bool {
        if self.proof {
            self.told_paid
        } else {
            self.told
        }
    }

    /// Whether the entry still has work a resume does, as [`JournalEntry::open`].
    fn open(&self) -> bool {
        !matches!(
            self.stage,
            Stage::AcceptanceOwed | Stage::Finished | Stage::Abandoned
        )
    }
}

/// What a resume returns (Paykit spec 7.2 steps 3 and 4): one function of the journal as
/// the call found it (`before`), the journal as the call leaves it (`after`), and what the
/// entries' turns gathered that the journal doesn't hold (`turns`). Nothing a step told
/// while the call ran is returned as it was told: a write kept at one step may go through
/// at another, and a batch kept from before the call may replace any entry when it is sent.
///
/// Each entry is reported at most once, in the journal's order:
///
/// - **still open:** its open form ([`open_outcome`]);
/// - **over, and not yet told** ([`AttemptFacts::told`]), whether this call ended it, by
///   its own step or by a batch sent at another's, or an earlier call that returned an
///   error or never returned did: exactly one outcome, read from the entry
///   ([`over_outcome`]); only an entry abandoned before its attempt reached the claim reports
///   none, having no attempt to report;
/// - **over and told already as it stands:** nothing. One told without a proof and since
///   found paid stands paid, and is reported paid once more.
fn resume_outcomes(after: &[EntryFacts], turns: &Turns) -> Vec<PayOutcome> {
    let mut out = Vec::new();
    for now in after {
        let outcome = if now.open() {
            // Open, or opened again by a payment found made after its failure was written.
            Some(open_outcome(now, turns))
        } else if now.told_as_it_stands() {
            None
        } else {
            over_outcome(now)
        };
        out.extend(outcome);
    }
    out
}

/// The open form of an entry still open: with the method's proof, paid and unwritten, or
/// what its step told of the write that waits (the app not current, the record held or
/// divergent); without one, pending, or uncertain where its step found the method's history
/// can't tell.
fn open_outcome(now: &EntryFacts, turns: &Turns) -> PayOutcome {
    let attempt_id = now.attempt_id;
    let told = turns.open.get(&attempt_id);
    if now.proof {
        match told {
            Some(
                told @ (PayOutcome::PaidUnreported { .. }
                | PayOutcome::RecordHeld { .. }
                | PayOutcome::RecordDivergent { .. }),
            ) => told.clone(),
            // Nothing told, or a doubt told before the proof came: paid and unwritten.
            None
            | Some(
                PayOutcome::Uncertain { .. }
                | PayOutcome::Paid { .. }
                | PayOutcome::Failed { .. }
                | PayOutcome::Pending { .. }
                | PayOutcome::PaidOutcomeUnwritten { .. }
                | PayOutcome::Resolved { .. },
            ) => PayOutcome::PaidOutcomeUnwritten { attempt_id },
        }
    } else {
        match told {
            Some(told @ PayOutcome::Uncertain { .. }) => told.clone(),
            // Nothing told, or a wait told of a write for a proof the entry no longer
            // holds: pending.
            None
            | Some(
                PayOutcome::PaidUnreported { .. }
                | PayOutcome::RecordHeld { .. }
                | PayOutcome::RecordDivergent { .. }
                | PayOutcome::Paid { .. }
                | PayOutcome::Failed { .. }
                | PayOutcome::Pending { .. }
                | PayOutcome::PaidOutcomeUnwritten { .. }
                | PayOutcome::Resolved { .. },
            ) => PayOutcome::Pending { attempt_id },
        }
    }
}

/// The one outcome of an entry over and not yet told, `None` only for one abandoned before
/// its attempt reached the claim. Read from the entry, in this order:
///
/// 1. the method's proof: paid, whatever else the entry says;
/// 2. an acceptance still owed, which only a payment `done` says was made owes, and the
///    user's resolution ([`JournalEntry::user_resolved`]): failed where the user said so,
///    and otherwise [`PayOutcome::Resolved`], never a failure the user didn't state;
/// 3. the method's word that the payment failed ([`AttemptFacts::method_failed`]): failed,
///    as the method reported;
/// 4. this app's own `failed` written on the claim while the attempt was before the
///    method's call, or the method's refusal of a first call
///    ([`JournalEntry::failed_before_call`]): failed, no payment started. The
///    mark is in the journal copy that write's batch carries, so it is there only once the
///    write applied, and only where the claim this call read didn't show the attempt
///    otherwise ended; a write that didn't go through leaves the entry open, and it is
///    reported pending;
/// 5. anything else: [`PayOutcome::Resolved`]. An entry finished with no such evidence, as
///    one another own app resolved on the claim, is never reported failed: a failure is
///    reported only on positive evidence that no payment was made (Paykit spec 7.2, 8.4).
fn over_outcome(now: &EntryFacts) -> Option<PayOutcome> {
    use crate::journal::UserResolution;
    let attempt_id = now.attempt_id;
    let failed = |reason: &str| PayOutcome::Failed {
        attempt_id,
        reason: reason.into(),
    };
    if now.proof {
        return Some(PayOutcome::Paid {
            attempt_id,
            proof_msg_id: now.proof_msg_id.clone(),
        });
    }
    if now.stage == Stage::Abandoned {
        return None;
    }
    // An acceptance is owed only for a payment `done` says was made: never a failure.
    if now.stage == Stage::AcceptanceOwed {
        return Some(PayOutcome::Resolved { attempt_id });
    }
    // The user's word that it was paid, or may have been, outlasts the method's failure; where
    // both the method and the user said it failed, the method's word is the reason.
    Some(match now.user_resolved {
        Some(UserResolution::Done | UserResolution::Unknown) => PayOutcome::Resolved { attempt_id },
        Some(UserResolution::Failed) | None if now.method_failed => failed(METHOD_FAILED),
        Some(UserResolution::Failed) => failed(USER_FAILED),
        None if now.failed_before_call => failed(NO_PAYMENT_STARTED),
        None => PayOutcome::Resolved { attempt_id },
    })
}

/// How what a resume returned disagrees with the journals it was computed from, `None` when
/// it agrees. Written apart from [`resume_outcomes`], as the rule that function must keep:
///
/// - no attempt is reported twice, and every outcome names an entry the journal holds;
/// - no entry the call found is gone before its end, and none lost a proof it held;
/// - an entry still open is reported once, in an open form that agrees with its proof;
/// - an entry over and not told as it stands is reported exactly once: paid with a
///   proof, resolved without one, failed only without a proof and on evidence that no
///   payment was made (the method's word, this app's failure before the call, or the
///   user's), never for an entry the user resolved as paid or unknown; one abandoned is not
///   reported;
/// - an entry over and told as it stands ([`EntryFacts::told_as_it_stands`]) isn't reported;
///   any other over entry is reported as above.
#[cfg(any(test, feature = "test-support"))]
fn resume_disagreement(
    before: &[EntryFacts],
    after: &[EntryFacts],
    out: &[PayOutcome],
) -> Option<String> {
    use crate::journal::UserResolution;
    let mut reported: HashMap<AttemptId, &PayOutcome> = HashMap::new();
    for outcome in out {
        if reported.insert(outcome.attempt_id(), outcome).is_some() {
            return Some(format!("{:?} is reported twice", outcome.attempt_id()));
        }
    }
    for was in before {
        if !after.iter().any(|now| now.attempt_id == was.attempt_id) {
            return Some(format!("{:?} is gone from the journal", was.attempt_id));
        }
    }
    for now in after {
        let attempt_id = now.attempt_id;
        let was = before.iter().find(|was| was.attempt_id == attempt_id);
        if !now.proof && was.is_some_and(|was| was.proof) {
            return Some(format!("{attempt_id:?} lost its proof"));
        }
        let outcome = reported.remove(&attempt_id);
        let agrees = if now.open() {
            match outcome {
                Some(
                    PayOutcome::PaidOutcomeUnwritten { .. }
                    | PayOutcome::PaidUnreported { .. }
                    | PayOutcome::RecordHeld { .. }
                    | PayOutcome::RecordDivergent { .. },
                ) => now.proof,
                Some(PayOutcome::Pending { .. } | PayOutcome::Uncertain { .. }) => !now.proof,
                Some(
                    PayOutcome::Paid { .. }
                    | PayOutcome::Failed { .. }
                    | PayOutcome::Resolved { .. },
                )
                | None => false,
            }
        } else if now.told_as_it_stands() || now.stage == Stage::Abandoned && !now.proof {
            // Told as it stands, or abandoned before its attempt reached the claim.
            outcome.is_none()
        } else {
            match outcome {
                Some(PayOutcome::Paid { .. }) => now.proof,
                Some(PayOutcome::Failed { .. }) => {
                    !now.proof
                        && now.stage == Stage::Finished
                        && match now.user_resolved {
                            Some(UserResolution::Failed) => true,
                            Some(UserResolution::Done | UserResolution::Unknown) => false,
                            None => now.method_failed || now.failed_before_call,
                        }
                }
                Some(PayOutcome::Resolved { .. }) => !now.proof,
                Some(
                    PayOutcome::Pending { .. }
                    | PayOutcome::Uncertain { .. }
                    | PayOutcome::PaidOutcomeUnwritten { .. }
                    | PayOutcome::PaidUnreported { .. }
                    | PayOutcome::RecordHeld { .. }
                    | PayOutcome::RecordDivergent { .. },
                )
                | None => false,
            }
        };
        if !agrees {
            return Some(format!(
                "{attempt_id:?} is reported {outcome:?}; its entry was {:?} and is {:?}, proof \
                 {}, told {}, resolved by the user {:?}",
                was.map(|was| was.stage),
                now.stage,
                now.proof,
                now.told,
                now.user_resolved
            ));
        }
    }
    reported
        .keys()
        .next()
        .map(|attempt_id| format!("{attempt_id:?} is reported and not in the journal"))
}

/// The wallet-wide stop a read of this app's own record carries in the reason it kept the
/// earlier copy: the Dispatch refusing the read with an answer [`stops_resume`] counts, which
/// the read keeps as its code. `None` for any other reason.
pub(crate) fn kept_read_stop(
    reason: &pubky_dispatch_client::app::KeptReason,
) -> Option<PaykitError> {
    let pubky_dispatch_client::app::KeptReason::SwitchRefused { code } = reason else {
        return None;
    };
    let error = PaykitError::Client(AppError::Switch(
        pubky_dispatch_client::error::ClientError::Switch {
            status: code.status(),
            code: *code,
            message: "the Dispatch refused the record's read".into(),
            current: serde_json::Value::Null,
            known_version: None,
            retry_after: None,
        },
    ));
    stops_resume(&error).then_some(error)
}

/// The Dispatch client's refusal of an attempt (Dispatch spec 3.6, 10.1, 12.3, 12.4), carried as
/// the client reports it: an alarm, an app that left or a recovery notice is not a wait, and
/// the caller tells them apart by the refusal.
pub(crate) fn held_back(refusal: ClientRefusal) -> PaykitError {
    PaykitError::Client(AppError::Refused(refusal))
}

/// How old, on the device clock, the highest service time verified may be and still be
/// taken as the latest seen for a payment's deadline (Paykit spec 8.4): older, the Dispatch is
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

/// A new proof for the outcome, or the original already held for this payment.
enum PaidProof {
    New(Box<SignedMessage>),
    /// The record supplies the original payment evidence. Any pending delivery
    /// remains with the outbox that holds the signed message (Paykit spec 7.2).
    Original(Box<StoredMessage>),
}

impl PaidProof {
    fn msg_id(&self) -> &str {
        match self {
            Self::New(proof) => proof.msg_id(),
            Self::Original(proof) => &proof.message().msg_id,
        }
    }

    fn hash(&self) -> [u8; 32] {
        match self {
            Self::New(proof) => proof.hash(),
            Self::Original(proof) => proof.hash(),
        }
    }
}

/// Whether an authenticated payer proof records the journaled payment.
fn matches_paid_proof(
    original: &StoredMessage,
    payer: [u8; 32],
    app: [u8; 16],
    payee: [u8; 32],
    refs: &[[u8; 32]],
    expected: &PaymentProof,
) -> bool {
    original.sender() == payer
        && original.cert().app_id() == app
        && original.message().to == payee
        && original.message().refs == refs
        && matches!(original.body(), Body::PaymentProof(held) if held == expected)
}

/// The payer's activity record's part of an outcome's batch.
enum OutcomeRecord {
    /// This write.
    Write(Box<(Vec<Condition>, OpKind)>),
    /// A correctly bound original proof is already present at this record revision.
    Compared(Condition),
    /// None: the record already holds what the outcome places.
    Unchanged,
    /// The record can't be updated now: the outcome waits.
    Held,
    /// The record is divergent and the user hasn't confirmed it: the outcome waits.
    Divergent([u8; 32]),
}

/// When a first handoff's attempt was answered: its service time, and the device's wall
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

/// Which handoff [`Paykit::final_payment_check`] judges.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Handoff {
    /// The first: nothing was submitted before.
    First {
        /// When its attempt was answered.
        answered: Answered,
        /// The user confirmed this selected payment to the blocked payee.
        blocked_confirmed: bool,
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
    /// A repeat past the idempotency retention, one that might not complete before its
    /// attempt is orphaned, or one under an Allowance that has expired: never made, and the
    /// attempt stays "may be paid".
    Lapsed,
    /// The client refused the payee's late Dispatch answer, such as an equivocation (Dispatch
    /// wire 3.3): nothing is paid, and the refusal is the caller's.
    Refused(Refusal),
}

/// Whether the payment method still keeps what it keeps for `retention` (`None` for good)
/// of a handoff journaled at `journaled_at_ms`, at the device clock `now` (Paykit spec 7.2,
/// "What a payment method declares"): its age counts from the journaling, and the retention
/// is half-open, so at `journaled_at_ms + retention` the history or the idempotency key may
/// be gone already. A clock now behind the journaling can't tell the age, which is outside
/// every finite retention: an empty history proves nothing then, and a repeat may find the
/// key forgotten. A retention kept for good holds whatever the clock.
pub(crate) fn retained(retention: Option<u64>, journaled_at_ms: u64, now: u64) -> bool {
    retention.is_none_or(|r| now.checked_sub(journaled_at_ms).is_some_and(|age| age < r))
}

/// Whether the Allowance an attempt was reserved under has expired by the later of the
/// device clock `now` and the highest service time verified, `service_time`.
fn allowance_expired(entry: &JournalEntry, now: u64, service_time: Option<u64>) -> bool {
    entry.allowance.is_some()
        && entry
            .allowance_expires_at_ms
            .is_some_and(|expiry| expiry <= now.max(service_time.unwrap_or(0)))
}

/// Whether `bound` has passed (Paykit spec 7.2, "Kept proofs"): it lies before the earlier
/// of the highest service time verified, `service`, and the device clock `now` plus 24
/// hours, so one far-future signed time can't pass it; without a verified service time
/// none has.
pub(crate) fn bound_passed(bound: u64, service: Option<u64>, now: u64) -> bool {
    service.is_some_and(|service| {
        bound < service.min(now.saturating_add(pubky_dispatch_core::records::DAY_MS))
    })
}

/// How many times the outcome's batch is built again after `409 conflict`.
const OUTCOME_TRIES: usize = 3;

/// The deadline the app computes itself for an obligation (Dispatch wire 6.8): the attempt's,
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
#[serde(deny_unknown_fields)]
pub(crate) struct ListAsk {
    #[serde(with = "pubky_dispatch_core::enc::serde_b64")]
    pub(crate) payee: [u8; 32],
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) app_id: Option<[u8; 16]>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) held_version: Option<ListVersion>,
    pub(crate) asked_at_ms: u64,
}

/// What a seller lookup answer that became ready at the fence tells, against the basis the
/// final payment check decided on ([`Paykit::payment_fence`]).
#[derive(Debug, PartialEq, Eq)]
enum FenceNews {
    /// The Dispatch's copy confirms the document the check decided on: the payment goes on
    /// with the answer.
    Confirms,
    /// No news, for a payment by hand: the same document, and no more known of the Dispatch's
    /// copy than the check had. The payment goes on on its basis.
    None,
    /// News, or an automatic payment's unconfirmed answer: the payment holds.
    Holds,
}

/// What the fence knows of a seller lookup answer that became ready after the final payment
/// check ([`fence_news`]).
#[derive(Clone, Copy, Debug)]
struct Answer {
    /// The Dispatch copy state of the identity the answer finished with.
    copy: SwitchCopy,
    /// Whether the Dispatch's answer learned nothing from any service: no verified copy, and so
    /// no freeze or version ([`pubky_dispatch_client::app::SwitchAnswer::learned_nothing`]),
    /// whatever state the finished identity carries. A failed lookup can still carry a copy
    /// or a freeze a service gave before it failed.
    learned_nothing: bool,
    /// Whether the Dispatch's answer confirms the document the check decided on
    /// ([`pubky_dispatch_client::app::SwitchAnswer::confirms`]).
    confirmed: bool,
}

/// The fence catches news since the final payment check (Paykit spec 7.2, "check the
/// destination"; Pubky Dispatch spec, Section 4.3). `same_document` says whether the answer
/// carries the signed document the check decided on, and `basis` what the check knew of the
/// Dispatch's copy. An answer with another document, or one that learned anything from a
/// service (a copy, read or not, or a freeze), is news. One with the same document whose
/// lookup learned nothing, against a basis with no Dispatch copy either, is none for a payment
/// by hand. An automatic payment holds on
/// anything but a confirming answer, as it always has.
fn fence_news(same_document: bool, basis: SwitchCopy, answer: Answer, by: Initiator) -> FenceNews {
    if answer.confirmed && same_document && answer.copy == SwitchCopy::Read {
        FenceNews::Confirms
    } else if by == Initiator::User
        && same_document
        && answer.learned_nothing
        && unread(answer.copy)
        && unread(basis)
    {
        FenceNews::None
    } else {
        FenceNews::Holds
    }
}

/// Whether `copy` is a Dispatch copy state known to carry no read copy. A state this crate
/// doesn't know reads as one that may: the fence holds on it.
fn unread(copy: SwitchCopy) -> bool {
    matches!(
        copy,
        SwitchCopy::Unavailable | SwitchCopy::Stale | SwitchCopy::Pending
    )
}

impl Paykit {
    /// The local write that journals `entry` in a batch. The entry is checked first, its
    /// claim key bound to what it pays as every read back binds it
    /// ([`JournalEntry::check_bound`], `state::Stored`), so no path persists a state
    /// recovery would refuse: one that fails is a bug, refused as a corrupt store, with
    /// nothing written.
    pub(crate) fn journal_write(&self, entry: &JournalEntry) -> Result<LocalWrite> {
        entry.check_bound(|input| self.app.blind(input))?;
        state::local_put(&key::journal(&entry.attempt()?), entry)
    }

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
    /// `paykit_contacts`, the deny list as read at most 10 minutes ago, or read first, decides: a
    /// request from a blocked payee is [`Refusal::Blocked`] until the user confirms paying
    /// it. A request whose record is divergent, or was seen so, is [`Refusal::Divergent`]
    /// until the user confirms it ([`Paykit::confirm_divergent`]), except in three cases
    /// that can't be paid anyway: one past its deadline goes on to the checks after the
    /// pause, its deadline refusing it at the latest; one carrying endpoints whose app the
    /// payee's `identity.json` no longer lists as current, and one requiring an app that
    /// copy lists as removed, are [`Refusal::Destination`], that copy read fresh, a read
    /// that fails being the refusal. After a provider loss, a request payable before it
    /// whose new claim is not SETTLED is [`Refusal::MayBePaid`] with
    /// [`PaymentDoubt::before_loss`] (Paykit spec 7.3). The user must confirm that exact
    /// doubt or state it was not paid with [`Paykit::confirm_not_paid_before_loss`].
    ///
    /// The call returns a [`PayResult`]: these errors are its `outcome`'s, and its
    /// `public_failures` come with them, as with any outcome.
    pub async fn pay_request(&mut self, request: &RequestRef, options: PayOptions) -> PayResult {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        let mut public_failures = Vec::new();
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        let outcome = Box::pin(self.pay_request_inner(
            request,
            options,
            Initiator::User,
            None,
            &mut public_failures,
        ))
        .await;
        let outcome = self.tell(outcome).await;
        PayResult {
            outcome,
            public_failures,
        }
    }

    /// [`Paykit::pay_request`] started as an automatic payment ([`Initiator::Automatic`]), for
    /// tests only (feature `test-support`). This exercises a first handoff after the attempt
    /// is journaled. Wallet callers use [`Paykit::pay_under_allowance`] to check and reserve
    /// the payment under the executor grant.
    ///
    /// # Errors
    ///
    /// As [`Paykit::pay_request`].
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub async fn pay_request_automatically(
        &mut self,
        request: &RequestRef,
        options: PayOptions,
    ) -> PayResult {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        let mut public_failures = Vec::new();
        let outcome = Box::pin(self.pay_request_inner(
            request,
            options,
            Initiator::Automatic,
            None,
            &mut public_failures,
        ))
        .await;
        let outcome = self.tell(outcome).await;
        PayResult {
            outcome,
            public_failures,
        }
    }

    /// Pay an eligible one-time request under this app's accepted Allowance. The
    /// signed opening claim, reservation and request association are one Dispatch batch.
    /// Before each new payment, the wallet must account for its complete payment-method
    /// history and the authenticated current ledger (Paykit spec 8.3).
    ///
    /// The returned outcome reports a refusal when the grant, ledger, payment history
    /// or current request cannot authorize the transfer, including when the payment
    /// would exceed a limit. Method, Dispatch and storage failures follow the ordinary
    /// payment request flow.
    ///
    /// Where the Allowance itself stops it, the refusal says whether that is for good:
    /// [`Refusal::AllowanceEnded`] (ended, or its terms expired), [`Refusal::AllowanceConflicted`],
    /// [`Refusal::Rejected`], [`Refusal::Usage`] (this app isn't its executor, or the request
    /// is another counterparty's) and [`Refusal::NotAutomatic`] (outside its terms, its
    /// lifetime limit used up with nothing reserved or to release, set up before a provider
    /// loss and never granted here, or a one-time request after a provider loss) stop
    /// automatic payment for good; [`Refusal::ScopeMissing`] stops it
    /// until the signer renews this app's certificate with the scope;
    /// [`Refusal::AllowanceConfirmationNeeded`] waits for the
    /// user ([`Paykit::confirm_allowance_executor`]); [`Refusal::NotNow`] waits for a later
    /// read or answer, such as an unblock of the counterparty, except after a provider loss
    /// once the user confirmed again, which this version doesn't lift (its reason says so).
    pub async fn pay_under_allowance(
        &mut self,
        request: &RequestRef,
        allowance: &[u8; 32],
    ) -> PayResult {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        let mut public_failures = Vec::new();
        let outcome = Box::pin(self.pay_request_inner(
            request,
            PayOptions::default(),
            Initiator::Automatic,
            Some(*allowance),
            &mut public_failures,
        ))
        .await;
        let outcome = self.tell(outcome).await;
        PayResult {
            outcome,
            public_failures,
        }
    }

    async fn pay_request_inner(
        &mut self,
        request: &RequestRef,
        options: PayOptions,
        by: Initiator,
        allowance: Option<[u8; 32]>,
        public_failures: &mut Vec<PublicAppFailure>,
    ) -> Result<PayOutcome> {
        // Finish the request reads before polling the payment and its outgoing messages.
        let plan =
            Box::pin(self.plan_request(request, options, by, allowance, public_failures)).await?;
        Box::pin(self.execute(plan)).await
    }

    async fn plan_request(
        &mut self,
        request: &RequestRef,
        options: PayOptions,
        by: Initiator,
        allowance: Option<[u8; 32]>,
        public_failures: &mut Vec<PublicAppFailure>,
    ) -> Result<Plan> {
        self.refuse_held_back()?;
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        self.require_pay()?;
        self.app
            .may_attempt(by == Initiator::Automatic)
            .map_err(held_back)?;
        self.refuse_while_payer_pass_owed().await?;
        let (target, record) = self.payer_request(request).await?;
        let (stored, body) = request_of(&record).ok_or(Refusal::NotFound)?;
        let (stored, body) = (stored.clone(), body.clone());
        if body.is_recurring() {
            return Err(Refusal::Recurring.into());
        }
        // No automatic one-time payment under the shared post-loss hold (Paykit spec 8.1).
        if by == Initiator::Automatic && self.shared_loss_hold().await? {
            return Err(Refusal::NotAutomatic(crate::loss::SHARED_LOSS_HOLD.into()).into());
        }
        if record.shows_paid() {
            return Err(Refusal::AlreadyPaid.into());
        }
        if record.lost_to_delete.is_some() {
            return Err(payment_history_lost().into());
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
        // seen that the payee may have cancelled it (Paykit spec 6). Three requests that can't
        // be paid anyway are refused for what stops them instead, as a list that couldn't be
        // paid anyway pauses nothing (Paykit spec 8.4): one past its deadline goes on to the
        // checks below, its deadline refusing it at the latest, which the service's time only
        // moves further past; one whose endpoints' app is no longer current is refused as the
        // destination check refuses it; and one requiring an app the payee removed is refused
        // for that.
        let deadline = body.attempt_deadline(None)?;
        if Box::pin(self.pause_at(&record_key)).await?.blocks_by_hand()
            && !self.deadline_passed(deadline)
        {
            Box::pin(self.refuse_unpayable_request(
                &request.payee,
                &stored,
                &body.destination,
                &options,
            ))
            .await?;
            Box::pin(self.refuse_while_paused(&[record_key])).await?;
        }
        let end_hold = self
            .pass_hold(
                &record_key,
                &record,
                options.possibly_cancelled_confirmed.as_ref(),
            )
            .await?;
        // A blocked payee's request is paid only once the user confirms it (Paykit spec 8.1).
        let blocked = self.denied_for_user(&request.payee).await?;
        let obligation = self.obligation(&request.payee, &request.request_hash, 0);
        let restored = self.restored_attempts(&obligation).await?;
        let claim = self
            .payable_claim(&obligation, &stored, None, deadline, restored, &options, by)
            .await?;
        let step = claim.as_ref().map_or(0, |view| view.step);
        // No attempt once the deadline is at or before the highest service time verified
        // (Paykit spec 7.1); the service refuses one from its own clock anyway. The claim is
        // read first: a payment made, or another wallet's open attempt, is what the request
        // shows, though its deadline has passed; then the request record, read again.
        if self.deadline_passed(deadline) {
            return Err(self.past_deadline(&target, None).await?);
        }
        let allowance = if let Some(key) = allowance {
            // An occurrence marked manual-only is never paid automatically, whatever else
            // holds (Paykit spec 8.3, "Manual only").
            if record.manual_only {
                return Err(Refusal::ManualOnly.into());
            }
            let reservation = Box::pin(self.allowance_reservation(&key, &request.payee)).await?;
            reservation.request(&record, claim.as_ref(), None)?;
            let hold = if blocked {
                Some(AutomaticHold::Blocked)
            } else if self.floor_holds(&stored).await? {
                Some(AutomaticHold::Floor(self.shared_floor().await?))
            } else if self.held(&record_key, &record).await? {
                Some(AutomaticHold::Restore)
            } else if self.pause_at(&record_key).await?.blocks_automatic() {
                Some(AutomaticHold::Paused)
            } else {
                None
            };
            if let Some(hold) = hold {
                return Err(hold.refusal().into());
            }
            Some(reservation)
        } else {
            None
        };
        let allowed = allowance.as_ref().and_then(|a| a.endpoints());
        let accepted = match (
            body.accepted_payment_endpoint_identifiers.as_deref(),
            allowed,
        ) {
            (Some(request), Some(terms)) => Some(
                request
                    .iter()
                    .filter(|id| terms.contains(id))
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            (request, terms) => request.or(terms).map(<[_]>::to_vec),
        };
        if accepted.as_ref().is_some_and(Vec::is_empty) {
            return Err(Refusal::Destination(
                "the request and Allowance share no permitted endpoint".into(),
            )
            .into());
        }
        // The amount to each endpoint, by the request's fixed rates under conversion terms
        // (Paykit spec 7.2 step 1); a one-time request has no period details to price it.
        let pricing =
            Pricing::of_request(&body.amount, *request, None, body.conversion.as_ref(), by);
        let Chosen {
            destination,
            carrier,
            reserve,
            bound,
            payment_amount,
            paid,
            price,
        } = self
            .request_destination(
                &request.payee,
                &stored,
                &body,
                &options,
                accepted.as_deref(),
                &pricing,
                &mut None,
                public_failures,
            )
            .await?;
        if blocked {
            let amount = paid.ok_or_else(|| {
                Refusal::Destination(
                    "Paykit spec 8.1: the blocked payment's transfer amount is unknown".into(),
                )
            })?;
            let shown = BlockedPayment {
                payee: request.payee,
                request: Some(*request),
                period: None,
                amount,
            };
            refuse_blocked(true, shown, options.blocked_confirmed.as_ref())?;
        }
        let entry = JournalEntry {
            user_started: by == Initiator::User,
            attempt_id: AttemptId::generate()?.to_text(),
            allowance: None,
            obligation,
            payee: request.payee,
            request_hash: Some(pubky_dispatch_core::enc::b64(&request.request_hash)),
            natural_key: target.natural_key.clone(),
            amount: KeptAmount::of(&body.amount),
            payment_amount: KeptAmount::of(&payment_amount),
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
            offline: None,
            restored: false,
            journaled_at_ms: self.now(),
            allowance_expires_at_ms: None,
            carrier: None,
            carrier_record: None,
            attempt_proof: None,
            failed_by_method: false,
            never_dispatched: false,
            user_paid: false,
            request: Some(KeptMessage::of(&stored)),
            record_held: false,
            transfer_bound: Some(bound),
            price,
            failed_on_claim: false,
            failed_before_call: false,
            user_resolved: None,
            billing_period: None,
        };
        Ok(Plan {
            entry,
            record: target,
            carrier,
            step,
            reserve,
            create: if allowance.is_some() || by == Initiator::User && !record.manual_only {
                Some(record)
            } else {
                None
            },
            allowance,
            on_record: true,
            by,
            unconfirmed: options.unconfirmed_identity_confirmed,
            blocked_confirmed: options.blocked_confirmed,
            end_hold,
            conditions: Vec::new(),
            record_writes: Vec::new(),
        })
    }

    /// Choose the request's destination under its accepted identifiers and pricing
    /// (Paykit spec 7.2, 8.1; Paykit wire 4.5). `accepted` may further restrict the request's
    /// identifiers, as an Allowance does.
    // Each input is a separate part of the choice, and the failures are this call's sink.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn request_destination(
        &mut self,
        payee: &[u8; 32],
        stored: &StoredMessage,
        body: &PaymentRequest,
        options: &PayOptions,
        accepted: Option<&[PaymentEndpointIdentifier]>,
        pricing: &Pricing<'_>,
        identity: &mut Option<PeerIdentity>,
        public_failures: &mut Vec<PublicAppFailure>,
    ) -> Result<Chosen> {
        let accepted_ids = accepted_request_endpoints(body, accepted);
        let accepted = accepted_ids.as_deref();
        let endpoints = match (&body.destination, stored.body()) {
            (_, Body::PaymentPeriodDetails(details)) => Some(
                details
                    .payment_endpoints
                    .as_ref()
                    .ok_or_else(PaykitError::corrupt)?,
            ),
            (RequestDestination::Endpoints(endpoints), _) => Some(endpoints),
            _ => None,
        };
        if let Some(endpoints) = endpoints {
            let allowed =
                |id: &PaymentEndpointIdentifier| accepted.is_none_or(|ids| ids.contains(id));
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
            let Some(first) = gathered.first() else {
                return Err(Refusal::NoDestination(
                    "Paykit spec 8.1: the request has no such endpoint".into(),
                )
                .into());
            };
            let period_destination = matches!(stored.body(), Body::PaymentPeriodDetails(_));
            let period_rates =
                matches!(&pricing.rates, Rates::Period(details) if !details.is_empty());
            if identity.is_none() && (period_destination || period_rates) {
                *identity = Some(
                    self.payee_identity(
                        payee,
                        Need::FreshCopy,
                        pricing.by,
                        options.unconfirmed_identity_confirmed,
                    )
                    .await??,
                );
            }
            // All endpoints in this message have the same sender. Skip its offer before
            // choosing a price if its certificate no longer counts (Paykit spec 4, 8.2).
            if let Some(peer) = identity.as_ref().filter(|_| period_destination) {
                crate::destination::current(
                    &peer.doc,
                    &stored.message().from.app_id,
                    &first.carrier,
                    self.clocks(),
                )
                .map_err(Refusal::NoDestination)?;
            }
            return self
                .choose(payee, gathered, pricing, options, identity.as_ref())?
                .ok_or_else(|| unsupported("the signed endpoints"));
        }
        let app = match &body.destination {
            RequestDestination::RequiredApp(app) => Some(*app),
            _ => options.list_app,
        };
        self.list_destination(payee, app, accepted, pricing, options, public_failures)
            .await
    }

    /// Pay without a request (Paykit spec 8.4): to the payee's Private Payment List, or its
    /// public endpoints when the user chose them. The attempt, its `payer_activity` record and
    /// a single-use endpoint's mark go in one batch, so two own apps can't both pay one
    /// single-use endpoint. When the payer's Dispatch is unavailable, the user can still
    /// pay an explicitly selected reusable public endpoint with current identity and
    /// deny-list evidence. A successful transfer stays in the journal until the
    /// original attempt and its payer activity can be recorded (Paykit spec 8.4).
    ///
    /// # Errors
    ///
    /// As [`Paykit::pay_request`]; [`PaykitError::Conflict`] when the `payment_lists` record
    /// changed meanwhile, after which the caller pays to the next usable endpoint. As there,
    /// they are the [`PayResult`]'s `outcome`.
    pub async fn pay_without_request(&mut self, payment: UnrequestedPayment) -> PayResult {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        let mut public_failures = Vec::new();
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        let outcome = Box::pin(self.pay_without_request_inner(payment, &mut public_failures)).await;
        let outcome = self.tell(outcome).await;
        PayResult {
            outcome,
            public_failures,
        }
    }

    async fn pay_without_request_inner(
        &mut self,
        payment: UnrequestedPayment,
        public_failures: &mut Vec<PublicAppFailure>,
    ) -> Result<PayOutcome> {
        self.refuse_held_back()?;
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        self.require_pay()?;
        self.app.may_attempt(false).map_err(held_back)?;
        self.refuse_while_payer_pass_owed().await?;
        let options = payment.options;
        // A payment to a blocked payee is made only once the user confirms it (Paykit spec
        // 8.1; v0.1 refused any payment to a blocked peer).
        let blocked = self.denied_for_user(&payment.payee).await?;
        let Chosen {
            destination,
            carrier,
            reserve,
            bound,
            payment_amount,
            paid,
            ..
        } = self
            .list_destination(
                &payment.payee,
                options.list_app,
                None,
                &Pricing::unrequested(&payment.amount),
                &options,
                public_failures,
            )
            .await?;
        if blocked {
            let amount = paid.ok_or_else(|| {
                Refusal::Destination(
                    "Paykit spec 8.1: the blocked payment's transfer amount is unknown".into(),
                )
            })?;
            let shown = BlockedPayment {
                payee: payment.payee,
                request: None,
                period: None,
                amount,
            };
            refuse_blocked(true, shown, options.blocked_confirmed.as_ref())?;
        }
        let attempt_id = AttemptId::generate()?;
        let obligation = self.payment_obligation(&attempt_id);
        let deadline = match self.app.read_claim(&obligation).await {
            Ok(_) => Some(
                self.recent_service_time(&obligation)
                    .await?
                    .saturating_add(UNREQUESTED_DEADLINE_MS),
            ),
            Err(error) if dispatch_unavailable(&error) => {
                if !matches!(options.mode, PaymentMode::Public(_))
                    || options.endpoint.as_ref() != Some(&destination.endpoint_id()?)
                    || destination.source()? != DestinationSource::Public
                    || destination.single_use
                    || reserve.is_some()
                {
                    return Err(Refusal::DispatchUnavailable(error.to_string()).into());
                }
                None
            }
            Err(error) => return Err(error.into()),
        };
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
            user_started: true,
            allowance: None,
            attempt_id: attempt_id.to_text(),
            obligation: self.payment_obligation(&attempt_id),
            payee: payment.payee,
            request_hash: None,
            natural_key: natural.clone(),
            amount: KeptAmount::of(&payment.amount),
            payment_amount: KeptAmount::of(&payment_amount),
            destination,
            deadline,
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
            offline: None,
            restored: false,
            journaled_at_ms: self.now(),
            allowance_expires_at_ms: None,
            carrier: None,
            carrier_record: None,
            attempt_proof: None,
            failed_by_method: false,
            never_dispatched: false,
            user_paid: false,
            request: None,
            record_held: false,
            transfer_bound: Some(bound),
            price: None,
            failed_on_claim: false,
            failed_before_call: false,
            user_resolved: None,
            billing_period: None,
        };
        if deadline.is_none() {
            return Box::pin(self.pay_public_offline(entry, carrier, &options)).await;
        }
        self.execute(Plan {
            entry,
            record: ActivityTarget::payer(natural),
            carrier,
            step: 0,
            reserve,
            create: Some(record),
            on_record: false,
            allowance: None,
            by: Initiator::User,
            unconfirmed: options.unconfirmed_identity_confirmed,
            blocked_confirmed: options.blocked_confirmed,
            end_hold: None,
            conditions: Vec::new(),
            record_writes: Vec::new(),
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
    /// updated as at any outcome. When recovery shows a different signed write at the held
    /// version, the payment stays [`PayOutcome::RecordHeld`] until the client accepts the
    /// matching write or a successor.
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
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        let outcome = Box::pin(self.repair_activity_record_inner(attempt_id)).await;
        self.tell(outcome).await
    }

    async fn repair_activity_record_inner(&mut self, attempt_id: &AttemptId) -> Result<PayOutcome> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
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
            // The outcome is written and the proof queued with it: the payment is reported
            // `Paid`, and a delivery failure, a stop included, is the outbox's to meet again.
            let _ = self.send_message(&msg_id).await;
        }
        Ok(PayOutcome::Paid {
            attempt_id: *attempt_id,
            proof_msg_id: entry.proof_msg_id.clone(),
        })
    }

    /// The journal, every entry read back bound to its key (`state::Stored`), as the
    /// method's word says it stands ([`JournalEntry::with_facts`]).
    pub(crate) async fn journal(&self) -> Result<Vec<JournalEntry>> {
        Ok(self
            .journal_with_facts()
            .await?
            .into_iter()
            .map(|(entry, _)| entry)
            .collect())
    }

    /// [`Paykit::journal`], each entry with the facts laid over it ([`AttemptFacts`]): the
    /// one read of the whole journal.
    pub(crate) async fn journal_with_facts(&self) -> Result<Vec<(JournalEntry, AttemptFacts)>> {
        let mut facts: HashMap<String, AttemptFacts> = self
            .local_scan::<AttemptFacts>(key::ATTEMPT_FACTS)
            .await?
            .into_iter()
            .collect();
        self.local_scan::<JournalEntry>(key::JOURNAL)
            .await?
            .into_iter()
            .map(|(_, entry)| {
                // A scan names each value by its key past the prefix: the attempt ID.
                let kept = facts.remove(&entry.attempt()?.to_text());
                Ok((entry.with_facts(kept.as_ref()), kept.unwrap_or_default()))
            })
            .collect()
    }

    /// The journal entry of `attempt`, read back checked and bound to its key: of that
    /// attempt, its claim key the one its payee and request, or its attempt for a payment
    /// without a request, blind to, its record the one its request or attempt names
    /// (`state::Stored`).
    ///
    /// # Errors
    ///
    /// A store failure, or an entry that doesn't read or isn't the attempt its key names.
    pub(crate) async fn journal_entry(&self, attempt: &AttemptId) -> Result<Option<JournalEntry>> {
        let facts = self.attempt_facts(attempt).await?;
        Ok(self
            .local_get::<JournalEntry>(&key::journal(attempt))
            .await?
            .map(|entry| entry.with_facts(facts.as_ref())))
    }

    /// What this app keeps beside `attempt`'s journal entry ([`AttemptFacts`]).
    async fn attempt_facts(&self, attempt: &AttemptId) -> Result<Option<AttemptFacts>> {
        self.local_get::<AttemptFacts>(&key::attempt_facts(attempt))
            .await
    }

    /// Keep what the payment method said of `entry`'s attempt, with the entry, in one commit
    /// ([`AttemptFacts`]): `change` makes the new facts from the ones held. Only the places
    /// that learn the method's word call it: the history pass, a payment's own answer, the
    /// method's refusal of a first call, the recheck and a reconcile. The facts only grow
    /// ([`AttemptFacts::grows_to`]): a change that would lose a proof, the method's failure
    /// or the entry told is refused as a corrupt store and writes nothing, in every build.
    pub(crate) async fn learn(
        &mut self,
        entry: &mut JournalEntry,
        change: impl FnOnce(&mut AttemptFacts),
    ) -> Result<()> {
        let attempt = entry.attempt()?;
        let held = self.attempt_facts(&attempt).await?.unwrap_or_default();
        let mut next = held.clone();
        change(&mut next);
        next.check()?;
        if !held.grows_to(&next) {
            return Err(PaykitError::corrupt());
        }
        *entry = entry.clone().with_facts(Some(&next));
        entry.check_bound(|input| self.app.blind(input))?;
        let mut tx = state::Tx::default();
        tx.put(&key::journal(&attempt), entry)?
            .put(&key::attempt_facts(&attempt), &next)?;
        self.local_commit(tx).await
    }

    /// A call returns `outcome` to the host: where its attempt's entry is over, it is marked
    /// told first ([`AttemptFacts::told`]), so a resume never reports it again. Where that mark
    /// can't be written, the call returns the error and the next resume reports the outcome
    /// ([`resume_outcomes`]).
    pub(crate) async fn tell(&mut self, outcome: Result<PayOutcome>) -> Result<PayOutcome> {
        let outcome = outcome?;
        self.mark_told_if_over(&outcome.attempt_id()).await?;
        Ok(outcome)
    }

    /// Mark `attempt` told where its entry is over; nothing where it is open or not journaled.
    pub(crate) async fn mark_told_if_over(&mut self, attempt: &AttemptId) -> Result<()> {
        if let Some(entry) = self.journal_entry(attempt).await?.filter(|e| !e.open()) {
            self.mark_told(&[(*attempt, entry.rail_proof.is_some())])
                .await?;
        }
        Ok(())
    }

    /// Mark the entries of `attempts` told, in one commit: a call returned each one's
    /// outcome once it was over ([`AttemptFacts::told`]), paid where it held the method's
    /// proof ([`AttemptFacts::told_paid`]).
    async fn mark_told(&mut self, attempts: &[(AttemptId, bool)]) -> Result<()> {
        let mut tx = state::Tx::default();
        for (attempt, paid) in attempts {
            let held = self.attempt_facts(attempt).await?.unwrap_or_default();
            let next = AttemptFacts {
                told: true,
                told_paid: held.told_paid || *paid,
                ..held.clone()
            };
            next.check()?;
            if !held.grows_to(&next) {
                return Err(PaykitError::corrupt());
            }
            tx.put(&key::attempt_facts(attempt), &next)?;
        }
        self.local_commit(tx).await
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

    /// Journal `entry`, checked first as [`Paykit::journal_write`] does: only a state a
    /// resume and a restore read back is persisted.
    async fn save(&mut self, entry: &JournalEntry) -> Result<()> {
        entry.check_bound(|input| self.app.blind(input))?;
        self.local_put(&key::journal(&entry.attempt()?), entry)
            .await
    }

    pub(crate) async fn set_stage(&mut self, entry: &mut JournalEntry, stage: Stage) -> Result<()> {
        if matches!(stage, Stage::Finished | Stage::Abandoned) {
            entry.end_before_the_call();
        }
        entry.stage = stage;
        self.save(entry).await
    }

    /// The attempts a restore left unresolved for the obligation, which make it "may be
    /// paid" (Paykit spec 7.2); an open entry of this app's otherwise is a payment in
    /// progress. A restored handoff the user resolved as paid with no claim to write it on,
    /// and one the method reported paid whose outcome isn't written yet, are
    /// [`Refusal::AlreadyPaid`].
    pub(crate) async fn restored_attempts(&self, obligation: &[u8; 32]) -> Result<Vec<AttemptId>> {
        let mut restored = Vec::new();
        for entry in self.journal().await? {
            if &entry.obligation != obligation {
                continue;
            }
            // The user said a restored handoff no claim showed was paid: its obligation is
            // paid, as a `done` on its claim would show (Paykit spec 7.2, 8.4).
            if entry.user_paid {
                return Err(Refusal::AlreadyPaid.into());
            }
            if !entry.open() {
                continue;
            }
            // A restored handoff the method reported paid holds its proof: paid, whether or
            // not its outcome is written yet (Paykit spec 7.2).
            if entry.restored && entry.rail_proof.is_some() {
                return Err(Refusal::AlreadyPaid.into());
            }
            if !entry.restored {
                return Err(Refusal::PaymentInProgress.into());
            }
            restored.push(entry.attempt()?);
        }
        restored.sort();
        Ok(restored)
    }

    /// The claim to attempt at, after its checks (Paykit spec 7.1, 7.2, 7.3): a
    /// SETTLED claim is paid; another open attempt is another wallet's; a pinned deadline
    /// other than this app's may already be paid, with no confirmation passing it. Otherwise
    /// the doubt, `restored` with the claim's and a provider loss's for `request`, or its
    /// Billing Period named by `period`, read now: a doubtful one is refused
    /// with it ([`PaymentDoubt`]) unless the user confirmed exactly it, and one a provider
    /// loss leaves in doubt is refused an automatic payment `by` whatever was confirmed.
    /// A Billing Period with unresolved pre-loss history also holds a manual payment.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn payable_claim(
        &mut self,
        obligation: &[u8; 32],
        request: &StoredMessage,
        period: Option<PeriodRef>,
        deadline: Option<u64>,
        restored: Vec<AttemptId>,
        options: &PayOptions,
        by: Initiator,
    ) -> Result<Option<ClaimView>> {
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
        // A SETTLED claim is refused above.
        let before_loss = self.loss_doubt(obligation, Some(request), period).await?;
        if period.is_some() && before_loss.is_some() {
            return Err(Refusal::MayBePaid {
                reason: "Paykit spec 7.3, 8.2: the Billing Period may have been paid early before the provider loss".into(),
                doubt: None,
            }.into());
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
            before_loss,
        };
        if before_loss.is_some() && by == Initiator::Automatic
            || doubt.doubtful() && options.may_be_paid_confirmed.as_ref() != Some(&doubt)
        {
            let reason = if !doubt.restored.is_empty() {
                "Dispatch spec 12.4: a restored attempt is unresolved"
            } else if before_loss.is_some() {
                crate::loss::LOSS_DOUBT
            } else {
                "Paykit spec 7.1: a doubtful claim"
            };
            return Err(Refusal::MayBePaid {
                reason: reason.into(),
                doubt: Some(Box::new(doubt)),
            }
            .into());
        }
        Ok(view)
    }

    /// Refused when the app the destination of `request` must come from can't give one
    /// whatever the user confirms (Paykit spec 4, 7.2, 8.1), judged on the payee's
    /// `identity.json` read fresh, as for a request's endpoint: a request carrying
    /// endpoints, as the destination check refuses it, when the app that sent it, whose
    /// endpoints they are, isn't current; a request requiring an app, when the payee has
    /// removed that app, which is never listed again (Dispatch spec 4.3). An app not listed yet
    /// isn't removed. A request whose destination any current app may give is not judged,
    /// and no copy is read for it. A read that fails is the refusal: no app is taken as
    /// current nor as removed without it.
    pub(crate) async fn refuse_unpayable_request(
        &mut self,
        payee: &[u8; 32],
        request: &StoredMessage,
        destination: &RequestDestination,
        options: &PayOptions,
    ) -> Result<()> {
        if let RequestDestination::Any = destination {
            return Ok(());
        }
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
        match destination {
            RequestDestination::RequiredApp(app) if identity.doc.lists_removed(app) => {
                Err(Refusal::Destination(
                    "Paykit spec 8.1: the request's required app is removed from the payee's \
                     identity.json"
                        .into(),
                )
                .into())
            }
            RequestDestination::Endpoints(_) => {
                let carrier = Carrier::Message(Box::new(request.clone()));
                let app_id = request.message().from.app_id;
                crate::destination::current(&identity.doc, &app_id, &carrier, self.clocks())
                    .map_err(|why| Refusal::Destination(why).into())
            }
            _ => Ok(()),
        }
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
    /// use, none of a list whose version a single-use endpoint was used at and, for a request
    /// that restricts them, one of its accepted identifiers, `allowed` (Paykit wire 4.5),
    /// are handed to the payment method, which chooses ([`Paykit::choose`]). In
    /// [`PaymentMode::Public`], the endpoints of the payee's public endpoints instead, from
    /// its current apps' records ([`crate::app_records`]), with the same restrictions. With
    /// no usable list, missing, expired or used up, the payee is asked for its current one
    /// ([`Paykit::ask_for_lists`]) and the refusal is [`Refusal::WaitingForList`]; a list in
    /// use that offers no endpoint the payment may use is [`Refusal::NoDestination`]; lists
    /// in use whose endpoints the method pays none of, and no expired or used-up list, are
    /// [`Refusal::UnsupportedEndpoint`], without asking. The list of an app the payee's
    /// `identity.json` lists in `apps`, paused as divergent ([`Paykit::confirm_divergent`]),
    /// with no other list to pay, is [`Refusal::Divergent`] naming its record, whatever
    /// certificate it was signed under, and its app isn't asked; the list of an app that
    /// copy doesn't list in `apps`, or lists as removed, or of a higher generation than that
    /// copy (Paykit wire 4.13), plays no part, paused or not; a request to the app of the
    /// latter still names the pair held.
    async fn list_destination(
        &mut self,
        payee: &[u8; 32],
        app: Option<[u8; 16]>,
        allowed: Option<&[PaymentEndpointIdentifier]>,
        pricing: &Pricing<'_>,
        options: &PayOptions,
        public_failures: &mut Vec<PublicAppFailure>,
    ) -> Result<Chosen> {
        let wanted = |id: &PaymentEndpointIdentifier| {
            options.endpoint.as_ref().is_none_or(|want| want == id)
                && allowed.is_none_or(|ids| ids.contains(id))
        };
        if let PaymentMode::Public(_) = options.mode {
            // The failures go into the call's list as each app is judged, so an error later in
            // the read still returns those found before it ([`PayResult`]).
            let (identity, mut records) = self
                .public_records(
                    payee,
                    app,
                    pricing.by,
                    options.unconfirmed_identity_confirmed,
                    public_failures,
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
            // An app whose record served now failed gave no endpoints (`public_records`), so
            // a request requiring it has no public destination (Paykit spec 4, 8.1), unless
            // its records conflict and it keeps an earlier one (Dispatch spec 4.2). Each failure
            // is in the refusal, and in the call's `PayResult` with any outcome.
            if gathered.is_empty() {
                let mut text =
                    String::from("Paykit spec 4: the payee has no usable public endpoint");
                for failure in public_failures.iter() {
                    text.push_str(&format!("; {failure}"));
                }
                return Err(Refusal::NoDestination(text).into());
            }
            return self
                .choose(payee, gathered, pricing, options, Some(&identity))?
                .ok_or_else(|| unsupported("the payee's public endpoints"));
        }
        // Over a `payment_lists` record seen deleted, no list is paid from: it waits (Paykit
        // spec 6; wire 5, "Incomplete protected-record contract"; `records::list_key_open`).
        let read = self.lists_read_of(payee).await?;
        // The version of each record's write selected from: every attempt on a list endpoint
        // is conditioned on it, never on a write the app refused (Dispatch spec 8.2).
        let versions: Vec<([u8; 16], RecordVersion)> =
            read.iter().map(|(l, v)| (l.app_id, *v)).collect();
        let mut lists: Vec<PaymentListRecord> = read.into_iter().map(|(l, _)| l).collect();
        lists.retain(|l| app.is_none_or(|a| a == l.app_id));
        // A list held over its record, as when the record didn't read when it came, replaces
        // the record's list once placed (Paykit wire 5.4), by the test placement uses. Every
        // list of the payee's held is taken, also over a record that doesn't read: the pair
        // of each app's list held over its record where it is newer (`held_over`), and of
        // each app whose record holds no readable list (`held_only`), never paid to.
        let own = self.identity();
        let mut held_over: Vec<([u8; 16], ListVersion)> = Vec::new();
        let mut held_only: Vec<([u8; 16], ListVersion)> = Vec::new();
        // The apps whose list record is paused: never asked for a newer list, by name or by
        // a request naming no app, which they could answer (Paykit spec 8.4, wire 4.14).
        let mut paused_apps: Vec<[u8; 16]> = Vec::new();
        // Each list held over a paused record that holds no readable list, with its record:
        // judged against the payee's identity.json below, like a readable list, before its
        // pause counts.
        let mut paused_held: Vec<([u8; 16], ListVersion, [u8; 32])> = Vec::new();
        // A held list that doesn't read, or is kept under another record's key, gives no
        // hint here and stops nothing: placing it reports it (Paykit wire 5.4).
        for (rest, kept) in self
            .local_scan_readable::<KeptMessage>(key::HELD_LISTS)
            .await?
        {
            let Ok(stored) = kept.stored(&own) else {
                continue;
            };
            let app_id = stored.message().from.app_id;
            if stored.sender() != *payee || app.is_some_and(|a| a != app_id) {
                continue;
            }
            let record = self.app.record_key(&list_target(payee, &app_id))?;
            if format!("{}{rest}", key::HELD_LISTS) != key::held_list(&record) {
                continue;
            }
            let Some(version) = crate::receive::list_version(&stored) else {
                continue;
            };
            match lists.iter().find(|l| l.app_id == app_id) {
                Some(list) => {
                    if crate::receive::next_list_record(Some(list.clone()), &stored).is_some() {
                        held_over.push((app_id, version));
                    }
                }
                None if Box::pin(self.pause_at(&record)).await?.blocks_by_hand() => {
                    paused_held.push((app_id, version, record));
                }
                None => held_only.push((app_id, version)),
            }
        }
        // A list kept over its record until the user confirms it, as one that came after
        // the record was paused, names that record too (Paykit spec 6): its app is judged
        // as one whose list is held over a paused record. A readable record's pause is
        // judged with its list below.
        for (app_id, version, record) in self.lists_kept_over(payee).await? {
            if app.is_some_and(|a| a != app_id)
                || lists.iter().any(|l| l.app_id == app_id)
                || held_only.iter().any(|(a, _)| *a == app_id)
            {
                continue;
            }
            // Held over the same paused record too: the newer pair is the one asked for.
            if let Some(held) = paused_held.iter_mut().find(|(a, _, _)| *a == app_id) {
                held.1 = held.1.max(version);
                continue;
            }
            if Box::pin(self.pause_at(&record)).await?.blocks_by_hand() {
                paused_held.push((app_id, version, record));
            }
        }
        // The newest pair the wallet holds of each app's list, installed or held over its
        // record, whether or not it may be paid: a request naming the app names it, and
        // every way of asking takes it from here (Paykit wire 4.14).
        let newest = |app_id: [u8; 16], installed: ListVersion| {
            held_over
                .iter()
                .find(|(app, _)| *app == app_id)
                .map_or(installed, |(_, held)| (*held).max(installed))
        };
        let mut installed: Vec<([u8; 16], ListVersion)> = lists
            .iter()
            .filter_map(|l| {
                crate::receive::list_version(&l.list).map(|v| (l.app_id, newest(l.app_id, v)))
            })
            .collect();
        installed.extend(held_only.iter().copied());
        // Only a current app's list counts (Paykit spec 8.4, 7.2): a removed app's list, of
        // whatever version it chose, would shadow the current apps' and then be refused by
        // the destination check. The payee's `identity.json` is the copy that check takes,
        // so a fresh one is read once here, before any list is judged paused: a list of an
        // app that copy doesn't list in `apps`, or lists as removed, is never usable,
        // whatever its upload holds,
        // so it plays no part, paused or not, and only a list that would otherwise be usable
        // pauses the payment (Paykit spec 8.4). A read that fails is the payment's refusal:
        // without it, no list is taken as a current app's, nor as a removed app's.
        let identity = if lists.is_empty() && held_only.is_empty() && paused_held.is_empty() {
            None
        } else {
            match self
                .payee_identity(
                    payee,
                    Need::FreshCopy,
                    pricing.by,
                    options.unconfirmed_identity_confirmed,
                )
                .await?
            {
                Ok(identity) => Some(identity),
                Err(refusal) => return Err(refusal.into()),
            }
        };
        // Nor does a list of a higher generation than that copy: an app-signed number is no
        // evidence of the identity's generation (Paykit wire 4.13). It stays installed, and a
        // request to its app names its pair, the one held (Paykit wire 4.14).
        let mut ahead: Vec<([u8; 16], ListVersion)> = Vec::new();
        if let Some(identity) = &identity {
            lists.retain(|l| {
                if identity.doc.listed_cert(&l.app_id).is_none()
                    || identity.doc.lists_removed(&l.app_id)
                {
                    return false;
                }
                match crate::receive::list_version(&l.list) {
                    Some(v) if v.generation() <= identity.doc.generation() => true,
                    Some(v) => {
                        ahead.push((l.app_id, newest(l.app_id, v)));
                        false
                    }
                    None => false,
                }
            });
        }
        // A list held over a paused record is judged the same: of an app that copy doesn't
        // count, it plays no part; of a higher generation, its pause counts for nothing, and
        // its app is asked naming the pair held. Only then does its pause stop the payment
        // and every request its app could answer (Paykit spec 8.4; wire 4.13, 4.14).
        let mut paused_held_records: Vec<[u8; 32]> = Vec::new();
        if let Some(identity) = &identity {
            for (app_id, version, record) in paused_held {
                if identity.doc.listed_cert(&app_id).is_none()
                    || identity.doc.lists_removed(&app_id)
                {
                    continue;
                }
                if version.generation() > identity.doc.generation() {
                    installed.push((app_id, version));
                    ahead.push((app_id, version));
                    continue;
                }
                paused_apps.push(app_id);
                paused_held_records.push(record);
            }
        }
        // A list whose record is divergent, or was seen so, may be stale: the upload may hold
        // a newer one, withdrawing or replacing a destination. None of its endpoints,
        // single-use or reusable, is paid until the user confirms it (Paykit spec 6); with no
        // other list to pay, the payment is refused naming those records. The certificate the
        // held list was signed under decides nothing here: the upload's list may be signed
        // under the one its current app is listed with now.
        //
        // A list held over its record, as when the record didn't read when it came, replaces
        // the record's list once placed: until then that payee app's list isn't paid to, so
        // nothing goes to a list a later one, a withdrawal included, replaces (Paykit wire
        // 5.4). A hold the record already holds, placed and not yet cleared, stops nothing:
        // the test is the one placement takes. A divergent record is named first: its held
        // list is placed only once the user confirms it. The pair each app is asked for is
        // the newest it holds, resolved above.
        let mut paused_lists = Vec::new();
        let mut usable = Vec::with_capacity(lists.len());
        // The apps whose list a newer one held over its record replaces: not paid to, and
        // asked naming the held pair.
        let mut replaced: Vec<([u8; 16], ListVersion)> = Vec::new();
        for list in lists {
            let record = self
                .app
                .record_key(&list_target(&list.payee, &list.app_id))?;
            if Box::pin(self.pause_at(&record)).await?.blocks_by_hand() {
                paused_lists.push(record);
                paused_apps.push(list.app_id);
                continue;
            }
            if let Some(pair) = held_over.iter().find(|(app, _)| *app == list.app_id) {
                replaced.push(*pair);
                continue;
            }
            usable.push(list);
        }
        let mut lists = usable;
        // Of the lists not paused, only one the destination check takes counts: the
        // certificate it was signed under passes, and counts in the same copy of the payee's
        // `identity.json`.
        if let Some(identity) = &identity {
            let clocks = self.clocks();
            lists.retain(|l| {
                let carrier = Carrier::Message(Box::new(l.list.clone()));
                crate::destination::current(&identity.doc, &l.app_id, &carrier, clocks).is_ok()
            });
        }
        // Taken after the read of the payee's identity.json: a list that expired while it
        // was read is expired here, and the payee asked for its current one, rather than
        // chosen and then refused by the destination check. Expiry is judged by the later of
        // the device clock and the highest verified service time (Paykit spec 7.2).
        let clocks = self.clocks();
        // Highest version first, then by App ID, so every payer device hands the method the
        // same for the same lists. Versions of two publishing apps count apart (Paykit wire
        // 4.13), so this order says nothing about which list is newer.
        lists.sort_by_key(|l| {
            let version = crate::receive::list_version(&l.list);
            (std::cmp::Reverse(version), l.app_id)
        });
        // The version held of each app's list, greater pair first, for a request that names it.
        let mut held: Vec<([u8; 16], ListVersion)> = Vec::new();
        // Whether a list in use, neither expired nor used up, offers an endpoint the payment
        // may use, and whether there is such a list.
        let mut offered = false;
        let mut live = false;
        // Whether some app's list has expired, which its current one may replace, and whether
        // one expired only by the service's time, the device clock behind it.
        let mut expired = false;
        let mut device_clock_behind = false;
        // The endpoints of the lists in use that the payment may use, in the order above and
        // then by identifier. A single-use endpoint used at a list's version consumes that
        // whole version (Paykit wire 5.4, spec 8.4): every endpoint of the list, single-use or
        // reusable, is left out, and the list is used up, whatever the method pays.
        let mut gathered = Vec::new();
        let mut used_up = false;
        for record in lists {
            let Body::PrivatePaymentList(list) = record.list.body() else {
                continue;
            };
            if !held.iter().any(|(app, _)| *app == record.app_id) {
                held.push((record.app_id, list.version));
            }
            // A used-up list is used up whatever the payment would use of it, expired or not:
            // none of its endpoints is a candidate, and a newer list may offer what this one
            // lacks.
            if record.used.iter().any(|u| u.version == list.version) {
                used_up = true;
                continue;
            }
            let accepted = record.list.accepted_at().unwrap_or(0);
            let until = list.usable_until(accepted);
            if clocks.past(until) {
                expired = true;
                device_clock_behind |= until > clocks.device;
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
                // Every endpoint of a list carries its record and the version of the write it
                // was read from: the attempt is conditioned on it, and its batch marks a
                // single-use endpoint used.
                let version = versions
                    .iter()
                    .find(|(a, _)| *a == record.app_id)
                    .map(|(_, v)| *v)
                    .ok_or_else(PaykitError::corrupt)?;
                let reserve = Some((record.clone(), version));
                gathered.push(Choice::new(
                    id,
                    &endpoint.payload,
                    destination,
                    carrier,
                    reserve,
                ));
            }
        }
        // A list held over a record that doesn't read is asked for by its pair, if its app
        // still counts in the payee's identity.json.
        let current = |app_id: &[u8; 16]| {
            identity.as_ref().is_none_or(|identity| {
                identity.doc.listed_cert(app_id).is_some() && !identity.doc.lists_removed(app_id)
            })
        };
        for pair in replaced
            .into_iter()
            .chain(held_only.into_iter().filter(|(app, _)| current(app)))
        {
            if !held.iter().any(|(app, _)| *app == pair.0) {
                held.push(pair);
            }
        }
        held.extend(ahead);
        // A current app of the payee's whose list the wallet holds none of that counts, or
        // whose installed list the destination check refuses: a request naming it, with no
        // pair, asks it too, beside the ones naming a pair, so a list that can be paid may
        // come (Paykit spec 8.4, wire 4.14).
        let others: Vec<[u8; 16]> = match (&identity, app) {
            (Some(identity), None) if !held.is_empty() || !paused_apps.is_empty() => identity
                .doc
                .body()
                .apps
                .iter()
                .map(|listed| listed.app_id)
                .filter(|app_id| {
                    !identity.doc.lists_removed(app_id)
                        && !held.iter().any(|(a, _)| a == app_id)
                        && !paused_apps.contains(app_id)
                })
                .collect(),
            _ => Vec::new(),
        };
        // Endpoints the conversion terms leave unusable refuse the payment for that, unless a
        // list has expired or is used up: a newer list may serve then, and the payee is asked
        // for it as below.
        let unusable_by_terms =
            match self.choose(payee, gathered, pricing, options, identity.as_ref()) {
                Ok(Some(chosen)) => return Ok(chosen),
                Ok(None) => None,
                Err(error) if matches!(error.refusal(), Some(Refusal::UnsupportedEndpoint(_))) => {
                    Some(error)
                }
                Err(error) => return Err(error),
            };
        if let Some(error) = unusable_by_terms {
            if !expired && !used_up {
                return Err(error);
            }
        }
        // Lists in use that offer only endpoints the method doesn't pay, and no app's list
        // expired or used up to ask again: a newer list needn't differ, so the payee isn't
        // asked. A list used up is asked again whatever the method pays of it (Paykit spec
        // 8.4, "No usable list").
        let unsupported_only = offered && !expired && !used_up;
        // A list still in use with no endpoint the payment may use isn't missing, expired or
        // used up: a newer one needn't differ, so the payee isn't asked, unless another list
        // is used up.
        let unusable = live && !offered && !used_up;
        // A paused list is neither missing, expired nor used up (Paykit spec 8.4): the payment
        // waits for the user's confirmation of it, naming its record, and no app whose list
        // is paused is asked for a newer one, since the upload may hold one. An app whose
        // list isn't paused, and merely expired or is used up, is asked as without a paused
        // list.
        if !paused_lists.is_empty() {
            if !options.quote_only && !held.is_empty() && !unsupported_only && !unusable {
                self.ask_for_lists(payee, app, &held, &installed, &[], &paused_apps)
                    .await?;
            }
            Box::pin(self.refuse_while_paused(&paused_lists)).await?;
        }
        if unsupported_only {
            return Err(unsupported("the payee's Private Payment List endpoints"));
        }
        // A list expired only by the service's time tells the user the device clock is
        // behind it, on every way out (Paykit spec 7.2).
        let note = if device_clock_behind {
            "; a list expired by the service's time: the device clock is behind it"
        } else {
            ""
        };
        if unusable {
            return Err(Refusal::NoDestination(format!(
                "Paykit spec 8.4: the payee's Private Payment List has no endpoint this payment \
                 may use{note}"
            ))
            .into());
        }
        if options.quote_only {
            let why = if device_clock_behind {
                "a current Private Payment List is needed to quote this endpoint; a list expired \
                 by the service's time: the device clock is behind it"
            } else {
                "a current Private Payment List is needed to quote this endpoint"
            };
            // A quote sends nothing, so nothing asks the payee for a list: the host shows it;
            // a payment asks for one.
            return Err(Refusal::HeldForUser(crate::HeldForUserReason::Unresolved(why)).into());
        }
        // With nothing to ask but a paused app, as a payment naming one, the payment waits for
        // the user's confirmation of its record, not for a list (Paykit spec 8.4).
        let asks_nothing = match app {
            Some(app) => paused_apps.contains(&app),
            None => held.is_empty() && others.is_empty() && !paused_apps.is_empty(),
        };
        if asks_nothing {
            Box::pin(self.refuse_while_paused(&paused_held_records)).await?;
        }
        self.ask_for_lists(payee, app, &held, &installed, &others, &paused_apps)
            .await?;
        Err(Refusal::WaitingForList {
            device_clock_behind,
        }
        .into())
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

    /// The endpoint a payment takes of `gathered` (Paykit spec 7.2): the first in the
    /// payment method's order ([`Paykit::rail_order`]) whose transfer check passes
    /// ([`Paykit::check_transfer`]), so a malformed or amount-incompatible payload falls to
    /// the next. With none taken: the error of a candidate the method couldn't judge, which
    /// may be payable once it can, so nothing is concluded from the others; else the first
    /// transfer refusal; else `None`, as when there are none or the method pays none of
    /// them. The choice is journaled with the attempt, and a resume pays it.
    pub(crate) fn choose(
        &self,
        payee: &[u8; 32],
        gathered: Vec<Choice>,
        pricing: &Pricing<'_>,
        options: &PayOptions,
        identity: Option<&PeerIdentity>,
    ) -> Result<Option<Chosen>> {
        if gathered.is_empty() {
            return Ok(None);
        }
        let ordered = self.rail_order(&gathered, Some(pricing.requested))?;
        let mut gathered: Vec<Option<Choice>> = gathered.into_iter().map(Some).collect();
        let mut refused = None;
        let mut unusable = None;
        for at in ordered.payable {
            let Some(choice) = gathered.get_mut(at).and_then(Option::take) else {
                continue;
            };
            let (amount, price, by_hand) =
                match self.price_choice(&choice, pricing, options, identity) {
                    Ok(Taken::Pay(amount, price)) => (amount, price, false),
                    Ok(Taken::ByHand(amount)) => (amount, None, true),
                    Ok(Taken::Unusable(why)) => {
                        unusable.get_or_insert(why);
                        continue;
                    }
                    Err(error) if matches!(error.refusal(), Some(Refusal::PricedByRate { .. })) => {
                        refused.get_or_insert(error);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
            let transfer_confirmed = (pricing.by == Initiator::User)
                .then_some(options.transfer_confirmed.as_ref())
                .flatten();
            match self.check_transfer(
                payee,
                &choice.destination,
                &amount,
                price.as_ref(),
                by_hand,
                transfer_confirmed,
            ) {
                Ok(bound) => {
                    return Ok(Some(Chosen {
                        destination: choice.destination,
                        carrier: choice.carrier,
                        reserve: choice.reserve,
                        paid: bound.paid_amount(&amount)?,
                        bound,
                        payment_amount: amount,
                        price,
                    }))
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
        if let Some(error) = refused {
            return Err(error);
        }
        // Only endpoints the conversion terms leave unusable: refused for that, not for a
        // method that pays none.
        unusable.map_or(Ok(None), |why| {
            Err(Refusal::UnsupportedEndpoint(why.into()).into())
        })
    }

    /// What a payment pays to `destination` (Paykit spec 7.2 step 1, 8.2), by `pricing`
    /// ([`conversion::price`]): a payment without a request, its own amount; a payment of a
    /// request to an endpoint of the requested asset no rate prices, the amount asked,
    /// rounded up to the endpoint's smallest unit; for one a rate prices, the amount
    /// computed, once the host authorizes this exact quote ([`PayOptions::rate_authorized`])
    /// for a payment by hand or an eligible own-policy period; for a request
    /// without conversion terms to an endpoint of another asset, the amount asked, only by
    /// hand ([`Taken::ByHand`]), and unusable for an automatic payment; else unusable.
    ///
    /// # Errors
    ///
    /// [`Refusal::PricedByRate`] carries the quote when this payment lacks its authorization.
    fn price_choice(
        &self,
        choice: &Choice,
        pricing: &Pricing<'_>,
        options: &PayOptions,
        identity: Option<&PeerIdentity>,
    ) -> Result<Taken> {
        let destination = &choice.destination;
        let Some(request) = pricing.request else {
            return Ok(Taken::Pay(pricing.requested.clone(), None));
        };
        let endpoint_id = destination.endpoint_id()?;
        let (priced, details) = pricing.price(&*self.rail, &endpoint_id, |signed| {
            let now = self.now();
            crate::periods::rates_current(signed, now, self.service_time())
                && identity.is_some_and(|peer| {
                    crate::destination::current_rate_source(&peer.doc, signed, now).is_ok()
                })
        });
        let (rate, amount) = match priced {
            Priced::Plain(_)
                if matches!(pricing.rates, Rates::Period(_))
                    && options.selected_period_rate().is_some_and(|id| {
                        !matches!(&choice.carrier, Carrier::Message(signed)
                            if matches!(signed.body(), Body::PaymentPeriodDetails(_)) && signed.message().msg_id == id)
                    }) =>
            {
                return Ok(Taken::Unusable(
                    "Paykit spec 8.2: the selected period rate does not price this endpoint",
                ));
            }
            Priced::Plain(_) | Priced::ByHand(_)
                if options.quote_only || options.rate_authorized.is_some() =>
            {
                return Ok(Taken::Unusable(
                    "Paykit spec 7.2: this endpoint is not priced by the requested signed rate",
                ));
            }
            Priced::Plain(amount) => return Ok(Taken::Pay(amount, None)),
            // Never paid automatically, whatever the method would transfer (Paykit spec 7.2
            // step 1): a method that converts by itself would report the requested amount.
            Priced::ByHand(_) if pricing.by == Initiator::Automatic => {
                return Ok(Taken::Unusable(conversion::BY_HAND_ONLY))
            }
            Priced::ByHand(amount) => return Ok(Taken::ByHand(amount)),
            Priced::Unusable(why) => return Ok(Taken::Unusable(why)),
            Priced::ByRate { .. } if pricing.under_allowance => {
                return Ok(Taken::Unusable(
                    "Paykit spec 8.3: an Allowance cannot pay at a rate",
                ));
            }
            Priced::ByRate { rate, amount } => (rate, amount),
        };
        let source = details.map_or(RateSource::Request, |signed| RateSource::PeriodDetails {
            msg_id: signed.message().msg_id.clone(),
        });
        let payload = destination.payload()?;
        let shown = PricedPayment {
            request,
            billing_period: pricing.period,
            rate_source_hash: details.map_or(request.request_hash, StoredMessage::hash),
            app_id: destination.app_id,
            endpoint_id,
            payload_hash: pubky_dispatch_core::enc::hash(payload.as_str().as_bytes()),
            requested: pricing.requested.clone(),
            source,
            rate,
            amount,
        };
        if shown.may_authorize_rate(pricing.by == Initiator::User)
            && options.rate_authorized.as_ref() == Some(&shown)
        {
            return Ok(Taken::Pay(
                shown.amount.clone(),
                Some(KeptPrice::of(&shown, details)),
            ));
        }
        Err(Refusal::PricedByRate {
            payment: Box::new(shown),
        }
        .into())
    }

    /// The refusal of a payment of the request whose record is at `target`,
    /// once its deadline has passed (Paykit spec 7.1), decided on the record the Dispatch holds,
    /// read again, never on a copy held: another own app may have rejected or cancelled the
    /// request, or the payee cancelled it, as the request's status shows before its expiry.
    /// [`Refusal::DeadlinePassed`] only for a record that is there, reads, is bound to this
    /// request's key, holds this request, and leaves it open, as the Dispatch's current write
    /// ([`Paykit::fresh_request`]); a closed one gives what closed it. The Dispatch holding none
    /// (`404`), or a newer write this app refused, is no evidence the request is open: it
    /// gives `otherwise`, as any other record does: the error the attempt met, or, without one,
    /// [`Refusal::NotFound`] for none and [`PaykitError::Conflict`] for one that doesn't read
    /// as this request. The caller has taken the claim's state first.
    ///
    /// # Errors
    ///
    /// The read's failure: a record that couldn't be read again is never taken as unchanged.
    pub(crate) async fn past_deadline(
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
    /// `app`, of that app, naming the pair `installed` holds of it whether or not that list
    /// may be paid; without, of each app in `held`, and of each app in `others`, a current
    /// app of the payee's with no list held that counts, naming the pair `installed` holds of
    /// it if any, or one request naming no app when none is held. An app in `paused`, whose
    /// record is paused, is never asked, and while one is, no request names no app (Paykit
    /// spec 8.4, wire 4.14). A request already asked of the same app at the same held version is
    /// outstanding, and not asked again, until a day has passed: the payee can't be forced to
    /// answer, and a request it dropped is asked once more after that, so a payer waits at
    /// most a day for one more chance. The request is queued before its mark is kept: a crash
    /// between the two asks again, rather than never.
    async fn ask_for_lists(
        &mut self,
        payee: &[u8; 32],
        app: Option<[u8; 16]>,
        held: &[([u8; 16], ListVersion)],
        installed: &[([u8; 16], ListVersion)],
        others: &[[u8; 16]],
        paused: &[[u8; 16]],
    ) -> Result<()> {
        self.refuse_while_sends_held(payee).await?;
        let held_of = |app: [u8; 16]| installed.iter().find(|(a, _)| *a == app).map(|(_, v)| *v);
        let asks: Vec<(Option<[u8; 16]>, Option<ListVersion>)> = match app {
            // An app whose record is paused is never asked (Paykit spec 8.4).
            Some(app) if paused.contains(&app) => Vec::new(),
            Some(app) => vec![(Some(app), held_of(app))],
            // A request naming no app would reach a paused app too: the others are named.
            None if held.is_empty() && !paused.is_empty() => others
                .iter()
                .map(|app| (Some(*app), held_of(*app)))
                .collect(),
            None if held.is_empty() => vec![(None, None)],
            None => held
                .iter()
                .map(|(app, v)| (Some(*app), Some(*v)))
                .chain(others.iter().map(|app| (Some(*app), held_of(*app))))
                .collect(),
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
            // What ends its outbox copy past its 30 days, in the commit that queues it
            // (Paykit spec 8.1).
            let kept = state::local_put(
                &key::queued_stop(signed.msg_id()),
                &crate::send_holds::QueuedStop::signed(
                    &signed,
                    crate::send_holds::HeldKind::ListRequest,
                    None,
                    None,
                )?,
            )?;
            let msg_id = self
                .app
                .queue_signed_with(crate::protocol::BACKUP_NAME, signed, &[kept])
                .await?;
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
            if let Err(error) = self.send_message(&msg_id).await {
                unless_stop(error)?;
            }
        }
        Ok(())
    }

    /// The transfer check (Paykit spec 7.2 step 1): the method transfers exactly the amount
    /// paid; the fee is the payment method's (Paykit spec 7.2 step 1). A transfer that
    /// passes is what the user asked for when they called pay, and is paid with no other
    /// consent. A destination failing it is paid only on the user's explicit choice of
    /// exactly what it would transfer: the method is asked again, and the destination and
    /// its transfer must be the ones the user confirmed ([`TransferQuote`]); otherwise it is
    /// refused with what it would transfer now, for the user to see. A payment made only by
    /// hand (`by_hand`, [`Priced::ByHand`]) is paid only so, even when the method reports
    /// exactly the amount handed to it. Returns the bound the attempt is journaled with,
    /// which the final payment check holds the method to again.
    fn check_transfer(
        &self,
        payee: &[u8; 32],
        destination: &Destination,
        amount: &PaymentAmount,
        price: Option<&KeptPrice>,
        by_hand: bool,
        confirmed: Option<&TransferQuote>,
    ) -> Result<TransferBound> {
        let endpoint_id = destination.endpoint_id()?;
        let payload = destination.payload()?;
        let transfer = self.rail.transfer(&endpoint_id, &payload, amount);
        let reason = transfer_refused(&transfer, amount)
            .or_else(|| by_hand.then_some(conversion::BY_HAND_ONLY));
        let Some(reason) = reason else {
            return Ok(TransferBound::Asked {});
        };
        let quote = TransferQuote {
            payee: *payee,
            app_id: destination.app_id,
            endpoint_id,
            payload_hash: pubky_dispatch_core::enc::hash(payload.as_str().as_bytes()),
            amount: amount.clone(),
            transfer,
        };
        if confirmed.is_some_and(|confirmed| confirms(confirmed, &quote))
            && (price.is_none() || quote.transfer.amount.as_ref() == Some(amount))
        {
            return Ok(TransferBound::Confirmed {
                amount: quote.transfer.amount.as_ref().map(KeptAmount::of),
            });
        }
        Err(Refusal::Transfer {
            reason: reason.into(),
            quote: Box::new(quote),
        }
        .into())
    }

    /// The transfer check again, at the final payment check just before the call, a
    /// repeat's included (Paykit spec 7.2 step 1): the method's transfer now of the amount
    /// paid, the one its host authorized for a payment priced by a rate, must still be
    /// within the bound the attempt was journaled with, so an amount that changed since the
    /// attempt was planned is never paid. A transfer that is what the user asked for
    /// passes; a confirmed one passes
    /// also when it is the one confirmed. For a payment permitted only by hand,
    /// [`Paykit::amount_holds`] requires the original selection and the transfer must
    /// match its confirmation. `Err` inside holds the refusal.
    fn transfer_holds(
        &self,
        entry: &JournalEntry,
        payee: &PeerIdentity,
        automatic_first: bool,
    ) -> Result<std::result::Result<(), Refusal>> {
        let by_hand = match self.amount_holds(entry, payee, automatic_first)? {
            Ok(by_hand) => by_hand,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let endpoint_id = entry.destination.endpoint_id()?;
        let payload = entry.destination.payload()?;
        let amount = entry.call_amount()?;
        let transfer = self.rail.transfer(&endpoint_id, &payload, &amount);
        let confirmed = match &entry.transfer_bound {
            Some(TransferBound::Confirmed { amount }) => Some(amount),
            Some(TransferBound::Asked {}) | None => None,
        };
        let reason = transfer_refused(&transfer, &amount)
            .or_else(|| by_hand.then_some(conversion::BY_HAND_ONLY));
        // Amounts compare as values, whatever their spelling (Paykit wire 1.2).
        let as_confirmed = match confirmed {
            Some(kept_amount) => {
                let kept_amount = kept_amount.as_ref().map(KeptAmount::amount).transpose()?;
                (entry.price.is_none() || transfer.amount.as_ref() == Some(&amount))
                    && transfer.amount == kept_amount
            }
            None => false,
        };
        let amount_changed = transfer.amount != entry.paid_amount()?;
        let reason = if amount_changed {
            Some("the transfer amount differs from the selected payment")
        } else {
            reason.filter(|_| !as_confirmed)
        };
        let Some(reason) = reason else {
            return Ok(Ok(()));
        };
        Ok(Err(Refusal::Transfer {
            reason: format!("{reason}, since the attempt was planned"),
            quote: Box::new(TransferQuote {
                payee: entry.payee,
                app_id: entry.destination.app_id,
                endpoint_id,
                payload_hash: pubky_dispatch_core::enc::hash(payload.as_str().as_bytes()),
                amount,
                transfer,
            }),
        }))
    }

    /// The call amount computed again, at the final payment check just before each call, a
    /// repeat's included (Paykit spec 7.2 step 1, 8.2): the amount the entry keeps must be
    /// the one the same rule gives now, with the payment method's unit now, so a kept amount
    /// that no rule gives, or one the method's unit no longer gives, is never paid. For a
    /// payment priced by a rate, the authorized rate must still be usable
    /// ([`rates_usable`]) and give exactly the originally authorized amount; for another payment of a request, the amount the selection gives without
    /// a rate must be the kept one, and one made only by hand ([`Priced::ByHand`]) is never
    /// a first handoff of an automatic payment; a payment without a request pays its own
    /// amount, which [`JournalEntry::check`] holds. `Ok` inside says whether the payment is
    /// made only by hand, which [`Paykit::transfer_holds`] then holds to its user's
    /// confirmation; `Err` inside holds the refusal, with what the payment would be now.
    fn amount_holds(
        &self,
        entry: &JournalEntry,
        payee: &PeerIdentity,
        automatic_first: bool,
    ) -> Result<std::result::Result<bool, Refusal>> {
        if entry.request_hash()?.is_none() {
            return Ok(Ok(false));
        }
        entry.selected_period_details(&self.identity())?;
        let endpoint_id = entry.destination.endpoint_id()?;
        let requested = entry.amount.amount()?;
        let paid = entry.call_amount()?;
        let Some(price) = &entry.price else {
            // The journal check holds a payment no rate prices to the requested amount, or,
            // to an endpoint of the requested asset, that amount rounded up: the selection
            // without rates gives it again.
            let priced = conversion::price(&*self.rail, &requested, None, &endpoint_id);
            let (now, by_hand) = match unpriced_now(priced, automatic_first)? {
                Ok(now) => now,
                Err(refusal) => return Ok(Err(refusal)),
            };
            if now == paid {
                return Ok(Ok(by_hand));
            }
            let payload = entry.destination.payload()?;
            return Ok(Err(Refusal::Transfer {
                reason: "the amount the endpoint's smallest unit gives changed since the attempt \
                         was planned"
                    .into(),
                quote: Box::new({
                    let transfer = self.rail.transfer(&endpoint_id, &payload, &now);
                    TransferQuote {
                        payee: entry.payee,
                        app_id: entry.destination.app_id,
                        transfer,
                        endpoint_id,
                        payload_hash: pubky_dispatch_core::enc::hash(payload.as_str().as_bytes()),
                        amount: now,
                    }
                }),
            }));
        };
        let selected = match self.current_rate_details(entry, payee)? {
            Ok(details) => details,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let mut shown = entry
            .priced_payment(selected.as_ref())?
            .ok_or_else(PaykitError::corrupt)?;
        let rate = price.rate()?;
        let priced = conversion::price(
            &*self.rail,
            &requested,
            Some(std::slice::from_ref(&rate)),
            &endpoint_id,
        );
        let holds = match priced {
            Priced::ByRate { amount, .. } if amount == paid => true,
            Priced::ByRate { amount, .. } => {
                shown.amount = amount;
                false
            }
            Priced::Plain(_) | Priced::ByHand(_) | Priced::Unusable(_) => false,
        };
        if holds
            && shown.may_authorize_rate(entry.user_started)
            && rates_usable(
                &shown.source,
                selected.as_ref(),
                self.now(),
                self.service_time(),
            )
        {
            return Ok(Ok(false));
        }
        Ok(Err(Refusal::PricedByRate {
            payment: Box::new(shown),
        }))
    }

    /// Steps 1 to 4 of the paying order for a planned attempt (Paykit spec 7.2).
    pub(crate) async fn execute(&mut self, plan: Plan) -> Result<PayOutcome> {
        let Plan {
            mut entry,
            record,
            carrier,
            step,
            reserve,
            mut create,
            on_record,
            allowance,
            by,
            unconfirmed,
            blocked_confirmed,
            end_hold,
            conditions: plan_conditions,
            record_writes,
        } = plan;
        let attempt_id = entry.attempt()?;
        let selected_request = entry.request_hash()?.map(|request_hash| RequestRef {
            payee: entry.payee,
            request_hash,
        });
        let selected_period = entry.period()?;
        let selected = entry.paid_amount()?.map(|amount| BlockedPayment {
            payee: entry.payee,
            request: selected_request,
            period: selected_period,
            amount,
        });
        let blocked_confirmed = selected
            .as_ref()
            .is_some_and(|selected| blocked_confirmed.as_ref() == Some(selected));
        // The attempt's `orphan_after` covers the rail's longest time and the pre-handoff
        // budget, as the rail declares it now (Dispatch spec 10.2).
        crate::paykit::check_orphan_window(&self.config, self.rail.as_ref())?;
        // The carrier is journaled with the entry, for the checks before any repeat.
        entry.keep_carrier(&carrier);
        entry.selected_period_details(&self.identity())?;
        // The own identity.json, read again and confirmed, before any write: no attempt
        // without a copy revalidated in the last 60 s (Paykit spec 4, Dispatch spec 12.6).
        self.own_identity_confirmed(true).await??;
        // The payee's identity.json, before any write: a payment the user started whose
        // payee's Dispatch couldn't confirm it needs the user's confirmation first, and an
        // automatic one isn't made (Paykit spec 7.2 step 1, Dispatch spec 4.3).
        let request_hash = entry.request_hash()?;
        let identity = self
            .check_destination(
                &entry.payee,
                &entry.destination,
                &carrier,
                request_hash.as_ref(),
                by,
                unconfirmed,
            )
            .await??;
        self.current_rate_details(&entry, &identity)??;
        self.activity_write_base(&self.activity_key(&record)?)
            .await?;
        let op = ClaimOp::attempt(
            entry.obligation,
            step,
            attempt_id.to_bytes(),
            self.config.orphan_after_s_for(&self.rail.declaration()),
            entry.deadline,
        );
        let mut conditions = plan_conditions;
        if on_record {
            let key = self.activity_key(&record)?;
            conditions.push(self.app.record_condition(&key).await?);
        }
        // A list endpoint's attempt is conditioned on the version of the `payment_lists`
        // write it was selected from: another own app's single-use payment since consumes the
        // whole version (Paykit spec 8.4; wire 5.4), also when this app saw that write and
        // refused it, which leaves the selected content but moves the record on.
        let reserve = match reserve {
            Some((list, version)) => {
                let key = self
                    .app
                    .record_key(&list_target(&list.payee, &list.app_id))?;
                conditions.push(Condition::RecordVersion { key, version });
                entry.destination.single_use.then_some(list)
            }
            None => None,
        };
        if let Some(original) = &mut create {
            if on_record {
                let (fresh, _) = self.fresh_request_stamped(&record).await?;
                if !matches!(fresh, crate::requests::Fresh::Held(ref value) if value.to_json()? == original.to_json()?)
                {
                    return Err(PaykitError::Conflict(
                        "the request changed while planning its payment".into(),
                    ));
                }
                if let Some(under) = &allowance {
                    original.allowance_id = Some(under.id.clone());
                } else {
                    original.manual_only = true;
                }
            }
        }
        if let Some(under) = &allowance {
            conditions.push(under.grant());
        }
        let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .op_if(op.kind(), conditions)
            .obligation_deadline(entry.obligation, own_deadline(entry.deadline));
        for write in record_writes {
            batch = batch.entry(write);
        }
        if let Some(end) = end_hold {
            // The user saw the payee may have cancelled it (Paykit spec 7.2): the hold ends
            // with the attempt, and stays when the attempt isn't made. A batch kept to send
            // again may be applied after a newer hold, which this confirmation doesn't end.
            batch = batch.local(state::local_put(&end.confirmation(), &true)?);
        }
        if let Some(under) = allowance {
            batch = Box::pin(under.reserve(self, &mut entry, batch)).await?;
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
        batch = batch.local(self.journal_write(&attempted)?);
        if Box::pin(self.period_recovery_held(&entry)).await? {
            return Err(Refusal::MayBePaid {
                reason: crate::loss::LOSS_DOUBT.into(),
                doubt: None,
            }
            .into());
        }
        // The attempt ID is journaled before the write (Paykit spec 7.2), once the batch is
        // built: a batch that can't be built leaves no open entry behind it.
        self.save(&entry).await?;
        // The attempt's age before its handoff (the pre-handoff budget,
        // [`crate::PRE_HANDOFF_BUDGET_MS`]) counts from before it is sent, on both device
        // clocks: the request, the answer and the answer's local commit are all in it, so
        // the age is never less than the time since the service accepted the attempt.
        let (answered_wall_ms, answered_at) = (self.now(), std::time::Instant::now());
        // The client checks the attempt's proof: only one that shows this app's own open
        // attempt lets money move (Paykit spec 7.2; Dispatch wire 6.5 "Binding").
        let submitted = self.submit(batch).await;
        let submitted = match submitted {
            Ok(submitted) => submitted,
            Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                self.set_stage(&mut entry, Stage::Abandoned).await?;
                // What the answer carries decides first: the claim it shows, with the open
                // attempt of another wallet, or the payment made (Paykit spec 7.1). Then the
                // request record the Dispatch holds, read again, which another own app may
                // have rejected or cancelled meanwhile, or the payee cancelled; a read that
                // fails is the error, never taken as the record unchanged. Only then the
                // deadline, at the answer's service time.
                if let Some(view) = claim_conflict(error.unkept()) {
                    self.refuse_claim(&view)?;
                }
                // A batch on a Billing Period writes its period record at the version read,
                // marking it `manual_only` when paid by hand, or is conditioned on it (Paykit spec 8.3, "Manual
                // only"): one another own app wrote since fails it, and is read again, so the
                // next attempt builds on that write, not on the same stale copy.
                if entry.period()?.is_some() {
                    let period = ActivityTarget::payer(entry.natural_key.clone());
                    self.app.read_record(&period.record_target()).await?;
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
        // The view the attempt's checked proof shows (Dispatch wire 6.5 "Binding").
        let claim = proof
            .map(|proof| ClaimView::from_value(&proof.body().state))
            .transpose()?;
        self.set_stage(&mut entry, Stage::Attempted).await?;
        if !Box::pin(self.keep_executor_reply(&entry, &submitted.proofs)).await? {
            return self
                .fail_attempt(
                    &mut entry,
                    Refusal::HeldForUser(crate::HeldForUserReason::AttemptCheck(
                        "the applied Allowance reservation could not be established",
                    )),
                )
                .await;
        }
        // Before any money moves, the gate every handoff passes (Paykit spec 4, 7.2 step 1).
        // The copies checked before the write are still fresh: this reads no network unless
        // they aged meanwhile (Dispatch spec 4.3). A failure writes `failed` instead of paying,
        // but for an attempt at a service the identity no longer names, which the move rules
        // resolve (Dispatch spec 12.5, 12.6): nothing is written there.
        let Some(claim) = claim else {
            return self
                .fail_attempt(
                    &mut entry,
                    Refusal::HeldForUser(crate::HeldForUserReason::AttemptCheck(
                        "no attempt proof",
                    )),
                )
                .await;
        };
        let Some(opened_at_ms) = claim
            .attempt(&attempt_id.to_bytes())
            .map(|attempt| attempt.opened_at)
        else {
            return self
                .fail_attempt(
                    &mut entry,
                    Refusal::HeldForUser(crate::HeldForUserReason::AttemptCheck(
                        "no attempt in its proof",
                    )),
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
        {
            Ok(doc) => doc,
            Err(held) => {
                return match first_handoff_hold(held) {
                    FirstHold::Return(error) => Err(error),
                    FirstHold::Fail(refusal) => self.fail_attempt(&mut entry, refusal).await,
                }
            }
        };
        // The entry stays `Attempted`, known unsubmitted, through the acceptance's journaling
        // and the final payment check: either can stop the payment, and a resume then writes
        // the attempt `failed` rather than taking it as possibly handed off. `Paying` is
        // journaled just before the payment method's call ([`Paykit::handoff`]). The
        // acceptance is signed and journaled now, and sendable only once that call is made.
        let acceptance = self.journal_acceptance(&mut entry).await?;
        Box::pin(self.handoff(
            entry,
            doc,
            by,
            unconfirmed,
            Handoff::First {
                answered,
                blocked_confirmed,
            },
            acceptance,
        ))
        .await
    }

    /// The own `identity.json` revalidated in the last [`REVALIDATION_MS`] (Paykit spec 4,
    /// "Fresh before paying"; Dispatch spec 12.6 "Residual risk"): with `refresh`, it is read
    /// again first, and without it only once its last confirmation is that old. A read that
    /// confirms the copy held, or a confirmation under 60 s old, lets the payment go on;
    /// `Missing`, `Older` and `Unavailable` confirm nothing, and an error refuses.
    pub(crate) async fn own_identity_confirmed(
        &mut self,
        refresh: bool,
    ) -> Result<std::result::Result<(), Refusal>> {
        if refresh || !self.own_identity_fresh() {
            // A Homeserver that can't be read now confirms nothing (`Unavailable`), and the
            // confirmation held still counts if it is fresh once the read is over. An error
            // is more: a copy that doesn't verify, a store that fails, or an alarm, such as
            // the own identity.json equivocating (Dispatch wire 3.3), and it stops the payment.
            let refreshed = self.app.refresh_identity().await;
            // The refresh can take a renewal into the own record.
            self.note_own_record_write(&refreshed);
            // A wallet-wide stop is returned as it is ([`unless_stop`]); any other failure
            // refuses this payment.
            if let Err(error) = refreshed {
                let why =
                    format!("Paykit spec 4: the own identity.json can't be read again: {error}");
                unless_stop(error)?;
                return Ok(Err(Refusal::IdentityUnavailable(why)));
            }
        }
        // Freshness is judged on the clock after the read, never on a time read before it: a
        // slow read that fails can't stretch the 60 s (Paykit spec 4).
        if self.own_identity_fresh() {
            return Ok(Ok(()));
        }
        Ok(Err(Refusal::IdentityUnavailable(
            "Paykit spec 4: the own identity.json wasn't confirmed in the last 60 s".into(),
        )))
    }

    /// Whether the own `identity.json` was confirmed less than [`REVALIDATION_MS`] before the
    /// device clock now. A confirmation the clock puts in the future is no evidence of a
    /// recent one: the clock went back.
    pub(crate) fn own_identity_fresh(&self) -> bool {
        self.app
            .identity_confirmation_age_ms()
            .is_some_and(|age| age < REVALIDATION_MS)
    }

    /// The latest service time the app has seen (Dispatch spec 10.1), for the deadline of a
    /// payment without a request (Paykit spec 8.4): the highest it verified when that is
    /// recent on the device clock, else the one a read of the payment's own claim, which a
    /// new attempt ID leaves empty, answers with. Deadlines run on the service's clock,
    /// never the device's: an old time would set a deadline that has passed already, and the
    /// device's could be off by the clock tolerance.
    ///
    /// # Errors
    ///
    /// The client's errors when the Dispatch can't be read now: no attempt could be written
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
            Refusal::NotNow(crate::NotNowReason::NoServiceTime(
                "Dispatch spec 10.1: no service time verified",
            ))
            .into()
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
        // that dropped `paykit_pay`, none is signed (Paykit spec 3.3); one signed before keeps its
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
    /// journaled for this handoff; without it (a resume, a repeat) the journaled one is
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
        let target = ActivityTarget::payer(entry.natural_key.clone());
        self.activity_write_base(&self.activity_key(&target)?)
            .await?;
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
            // twice once it can be sent. It reports a handed off payment, so it goes while
            // sends to the payee are held; that evidence is kept with the journal entry,
            // before the message can be queued (Paykit spec 8.1).
            entry.check()?;
            let mut tx = state::Tx::default();
            tx.put(&key::journal(&entry.attempt()?), entry)?.put(
                &key::send_exemption(signed.msg_id()),
                &crate::send_holds::SendExemption::handed_off(signed.msg_id()),
            )?;
            self.local_commit(tx).await?;
            self.app.queue_signed(signed).await?;
        }
        entry.acceptance_released = true;
        self.save(entry).await?;
        if let Some(msg_id) = entry.acceptance_msg_id.clone() {
            // Not awaited for paying: a delivery failure leaves it queued.
            if let Err(error) = self.send_message(&msg_id).await {
                unless_stop(error)?;
            }
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
        // The acceptance of a payment already made records that payment: none goes, nor is
        // placed, while the request record is paused, by the one gate of the outcome
        // (Paykit spec 6); the entry stays owed, for a resume after the user's confirmation,
        // which sends it also over a view still divergent.
        if Box::pin(self.handed_off_write_waits(&key)).await? {
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
                let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
                    .entry(write)
                    .local(self.journal_write(&finished)?);
                match self.submit(batch).await {
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
    async fn handoff(
        &mut self,
        mut entry: JournalEntry,
        payee: PeerIdentity,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        kind: Handoff,
        acceptance: Option<SignedMessage>,
    ) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        let payment = RailPayment {
            endpoint_id: entry.destination.endpoint_id()?,
            payload: entry.destination.payload()?,
            amount: entry.call_amount()?,
            idempotency_key: attempt_id,
            payment_reference: entry.payment_reference.clone(),
        };
        // Nothing is submitted unless the final payment check passes. Held, the attempt
        // stays open for a resume to decide: a first handoff is still `Attempted` then,
        // known unsubmitted, and the resume writes it `failed`.
        let (check, payee) = self
            .final_payment_check(&entry, payee, by, confirmed, kind)
            .await?;
        match check {
            FinalCheck::Pay => {}
            FinalCheck::Hold => return Ok(PayOutcome::Pending { attempt_id }),
            // The method may have forgotten the key, the repeat may not complete in time, or
            // its Allowance has expired: the repeat isn't made, and the attempt stays "may be
            // paid" (Paykit spec 7.2, 8.3).
            FinalCheck::Lapsed => return Ok(PayOutcome::Uncertain { attempt_id }),
            // A refused answer stops the payment. A first handoff submitted nothing, so it
            // is written `failed` and refused with the answer's refusal; a repeat may follow
            // a handoff that paid, so it stays "may be paid", for a receipt or the user.
            FinalCheck::Refused(refusal) => {
                return match kind {
                    Handoff::First { .. } => self.fail_attempt(&mut entry, refusal).await,
                    Handoff::Repeat { .. } => Ok(PayOutcome::Uncertain { attempt_id }),
                }
            }
        }
        // Starting to pay is journaled just before the payment begins (Paykit spec 7.2): from
        // here on it may be paid. A repeat's entry is `Paying` already.
        if matches!(kind, Handoff::First { .. }) {
            self.set_stage(&mut entry, Stage::Paying).await?;
        }
        match Box::pin(self.payment_fence(&entry, payee, by, confirmed, kind)).await {
            Ok(FinalCheck::Pay) => {}
            Ok(stopped) => return self.stopped_before_the_call(entry, kind, stopped).await,
            Err(error) => return self.unread_at_the_fence(entry, kind, error).await,
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
            if let Err(error) = self.release_acceptance(&mut entry, acceptance).await {
                unless_stop(error)?;
            }
            None
        } else {
            acceptance
        };
        match outcome {
            Ok(outcome) => self.take_outcome(&mut entry, outcome, acceptance).await,
            // A refusal is authoritative: nothing was and nothing will be submitted, so a
            // first handoff is definitively not paid, written `failed`, and may be replaced
            // (Paykit spec 7.2). A repeat's refusal says nothing of the handoff before it,
            // which may have paid: the attempt stays open.
            Err(RailError::Refused(why)) if matches!(kind, Handoff::First { .. }) => {
                // No payment started, kept with the stage: a restart before the outcome is
                // written reports it failed. Not the method's failure, whose history the
                // recheck would ask again: nothing was submitted to ask about.
                entry.failed_before_call = true;
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

    /// A handoff stopped after its final payment check, before the payment method's call:
    /// a first handoff as [`Paykit::stopped_at_the_fence`] leaves it; a repeat stays open,
    /// as the final payment check leaves it: [`PayOutcome::Pending`] for a hold, and
    /// "may be paid" otherwise, as the handoff before it may have paid.
    async fn stopped_before_the_call(
        &mut self,
        entry: JournalEntry,
        kind: Handoff,
        stopped: FinalCheck,
    ) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        match (kind, stopped) {
            (Handoff::First { .. }, stopped) => self.stopped_at_the_fence(entry, stopped).await,
            (Handoff::Repeat { .. }, FinalCheck::Hold) => Ok(PayOutcome::Pending { attempt_id }),
            (Handoff::Repeat { .. }, _) => Ok(PayOutcome::Uncertain { attempt_id }),
        }
    }

    /// A handoff whose fence failed, before the payment method's call: a read of the Dispatch,
    /// the store or the client failed, so nothing shows whether the payment may go now. The
    /// failure is the caller's to see, never a pending payment. A repeat's entry stays
    /// `Paying`, as the handoff before it may have paid, and the error is returned.
    ///
    /// A first handoff submitted nothing: it is written back to `Attempted`, known
    /// unsubmitted, then written `failed`, as a refusal at the fence writes it (Paykit spec
    /// 7.2 step 1), so a new payment may be made at once. A refusal is returned as the
    /// failed attempt's cause; any other error as it is. The failed write needs the Dispatch too: when it
    /// can't be made, the entry stays `Attempted` and the fence's error is returned; a
    /// resume writes the attempt `failed` later, and until then a new payment of the same
    /// obligation is refused as in progress.
    async fn unread_at_the_fence(
        &mut self,
        mut entry: JournalEntry,
        kind: Handoff,
        error: PaykitError,
    ) -> Result<PayOutcome> {
        // A wallet-wide stop is returned as it is: nothing more is written.
        if !matches!(kind, Handoff::First { .. }) || stops_resume(&error) {
            return Err(error);
        }
        self.set_stage(&mut entry, Stage::Attempted).await?;
        match self.write_outcome(&mut entry, false, false).await {
            // Written failed: a refusal is the failed attempt's cause; any other error, such
            // as the Dispatch's own answer with its code and retry time, or a store failure, is
            // returned as it is.
            Ok(_) => match error.refusal() {
                Some(why) => Err(Refusal::AttemptFailed(Box::new(why.clone())).into()),
                None => Err(error),
            },
            // The failed write didn't go: the entry is `Attempted` still.
            Err(write) => {
                unless_stop(write)?;
                Err(error)
            }
        }
    }

    /// A first handoff the fence stopped after `Paying` was journaled: nothing was
    /// submitted, so the entry is written back to `Attempted`, known unsubmitted, as a held
    /// final check leaves it. Then, with nothing paying, a lookup answer the fence saw come
    /// is taken: one the client refuses, such as an equivocation (Dispatch wire 3.3), writes
    /// the attempt `failed`, as a refused answer before `Paying` does. `Attempted` rather
    /// than `failed` at once: it is one local commit, the state a held check leaves, and a
    /// resume writes the open attempt `failed` from it ("no payment started").
    ///
    /// The write-back can be lost, to a crash or a store failure: the entry then stays
    /// `Paying`, which a restart reads as possibly handed off, resolved by the method's
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

    /// The method's outcome of `entry`'s handoff, journaled and reported. `acceptance` is
    /// the one this handoff signed, released once a payment made is journaled.
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
                // The proof is kept first: nothing after it can lose the payment. Then
                // the acceptance goes, the one signed for this handoff (Paykit spec 7.2
                // step 2); one that can't be queued now is left to the report.
                self.learn_paid(entry, proof, paid_at_ms).await?;
                if let Err(error) = self.release_acceptance(entry, acceptance).await {
                    unless_stop(error)?;
                }
            }
            RailOutcome::Failed => self.learn_failed(entry, true).await?,
            RailOutcome::Pending => {
                // The method shows the payment running, so its acceptance may go (Paykit spec
                // 7.2 step 2); one the handoff couldn't queue is queued now, best effort, at
                // every resume that finds it still pending.
                if let Err(error) = self.release_acceptance(entry, acceptance).await {
                    unless_stop(error)?;
                }
                return Ok(PayOutcome::Pending { attempt_id });
            }
        }
        self.report(entry).await
    }

    /// Steps 3 and 4: the outcome, the activity record and the proof, in one batch, then
    /// the proof is sent. A failure to write a payment made is reported as
    /// [`PayOutcome::PaidOutcomeUnwritten`], never as the payment's error (Paykit spec 7.2),
    /// unless it stops the wallet.
    pub(crate) async fn report(&mut self, entry: &mut JournalEntry) -> Result<PayOutcome> {
        Box::pin(self.report_as(entry, false)).await
    }

    /// [`Paykit::report`] for a resume: a failure of the Dispatch client to write a payment made
    /// is returned ([`judged_at_resume`]), so the resume reports the entry
    /// [`PayOutcome::PaidOutcomeUnwritten`] and decides whether the failure is a wait or one
    /// the host must act on ([`entry_waits`]).
    async fn resume_report(&mut self, entry: &mut JournalEntry) -> Result<PayOutcome> {
        Box::pin(self.report_as(entry, true)).await
    }

    async fn report_as(&mut self, entry: &mut JournalEntry, strict: bool) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        let paid = entry.rail_proof.is_some();
        if entry.offline.is_some() {
            if !paid {
                self.set_stage(entry, Stage::Finished).await?;
                return Ok(PayOutcome::Failed {
                    attempt_id,
                    reason: METHOD_FAILED.into(),
                });
            }
            match self.record_offline_attempt(entry).await {
                Ok(true) => {}
                Ok(false) => return Ok(PayOutcome::PaidOutcomeUnwritten { attempt_id }),
                Err(error) if strict && judged_at_resume(&error) => return Err(error),
                Err(error) => {
                    unless_stop(error)?;
                    return Ok(PayOutcome::PaidOutcomeUnwritten { attempt_id });
                }
            }
        }
        // Recording an Allowance outcome also authenticates the current ledger. Keep
        // that future off the wallet stack when resuming an interrupted handoff.
        match Box::pin(self.write_outcome(entry, paid, false)).await {
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
                // An app that isn't current can write nothing (Paykit spec 7.3), and the
                // Dispatch's answer stops the wallet as a whole ([`stops_resume`]): it is
                // returned, the entry left as it is, with the payment made in the journal.
                return Err(error);
            }
            // Money moved: a failure to write it is never the payment's error (Paykit spec
            // 7.2), unless it stops the wallet; the journal keeps the proof for a resume.
            Err(error) if strict && judged_at_resume(&error) => return Err(error),
            Err(error) if paid => {
                unless_stop(error)?;
                return Ok(PayOutcome::PaidOutcomeUnwritten { attempt_id });
            }
            Err(error) => return Err(error),
        }
        // As the entry holds it once written: a batch kept from before may have finished it
        // as a payment made while this step was writing a failure.
        if entry.rail_proof.is_none() {
            return Ok(PayOutcome::Failed {
                attempt_id,
                reason: METHOD_FAILED.into(),
            });
        }
        if let Some(msg_id) = entry.proof_msg_id.clone() {
            // After the payment and its outcome's batch, which finished the entry: the payment
            // is reported `Paid`, as nothing later reports a finished entry, and a delivery
            // failure, a stop included, leaves the proof queued for the outbox to meet again.
            let _ = self.send_message(&msg_id).await;
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
        if paid && !self.payment_base_changed(entry).await? {
            // A payment made waits for the user's confirmation of a paused payer record,
            // before its acceptance is released: nothing of it goes over that record until
            // then, and then also over a view still divergent (Paykit spec 6).
            let record = self.activity_key(&ActivityTarget::payer(entry.natural_key.clone()))?;
            if Box::pin(self.handed_off_write_waits(&record)).await? {
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
                unless_stop(error)?;
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
                // Read through the method's word: an entry an older write finished without
                // the proof this outcome carries reads paid and open again, so only an
                // outcome that holds what this one writes is taken as done.
                if matches!(kept.stage, Stage::Finished | Stage::AcceptanceOwed) {
                    *entry = kept;
                    return Ok(Written::Done);
                }
            }
            // The record's update is settled first: without it nothing is written, `done`
            // included (Paykit spec 7.2 step 3).
            let proof = if paid {
                Box::pin(self.outcome_proof(entry, &target)).await?
            } else {
                None
            };
            let decision = if paid && proof.is_none() {
                OutcomeRecord::Held
            } else {
                self.outcome_record(entry, &target, paid, proof.as_ref(), overwrite)
                    .await?
            };
            let mut compare = None;
            let record_write = match decision {
                OutcomeRecord::Write(write) => Some(*write),
                OutcomeRecord::Compared(condition) => {
                    compare = Some(condition);
                    None
                }
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
            let step = match entry
                .step
                .filter(|_| !matches!(proof, Some(PaidProof::Original(_))))
            {
                Some(step) => Some(step),
                None => match self.attempt_state(entry).await? {
                    Some((step, Outcome::Open | Outcome::Unknown)) => Some(step),
                    _ => None,
                },
            };
            let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME);
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
            if let Some(condition) = compare {
                batch = batch.when(condition);
            }
            if let Some(write) = record_write {
                batch = batch.entry(write);
            }
            if step.is_some() {
                batch = Box::pin(self.executor_outcome(entry, paid, batch)).await?;
            }
            let mut finished = entry.clone();
            if paid {
                finished.never_dispatched = false;
            } else {
                finished.end_before_the_call();
                // This app's own `failed` on the claim, for an attempt of this installation
                // still before the method's call, or a first call the method refused: no
                // payment started for it.
                finished.failed_before_call = entry.failed_before_call
                    || step.is_some()
                        && !entry.failed_by_method
                        && !entry.restored
                        && matches!(entry.stage, Stage::Recorded | Stage::Attempted);
            }
            // An acceptance still owed keeps the entry where a resume visits it: the outcome
            // is written, the acceptance not yet sent or placed.
            finished.stage = if paid && entry.owes_acceptance()? {
                Stage::AcceptanceOwed
            } else {
                Stage::Finished
            };
            finished.record_held = false;
            finished.proof_msg_id = proof.as_ref().map(|p| p.msg_id().to_owned());
            batch = batch.local(self.journal_write(&finished)?);
            if batch.is_empty() {
                if let Some(PaidProof::New(proof)) = proof {
                    self.app.queue_signed(*proof).await?;
                }
                self.save(&finished).await?;
                *entry = finished;
                return Ok(Written::Done);
            }
            if let Some(PaidProof::New(proof)) = proof {
                batch = batch.queue(*proof);
            }
            match self.submit(batch).await {
                Ok(result) => {
                    Box::pin(self.keep_executor_reply(entry, &result.proofs)).await?;
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
    ///   and the proof, and written as a create (`{0, 0}`, which the Dispatch refuses if a
    ///   record is there by then). A delete isn't a write that drops a role (Paykit wire
    ///   5.3, "Seen roles"); a reader that saw a role the rebuilt record lacks, such as a
    ///   receipt, ignores and flags it, and keeps what it saw.
    /// - **a value that doesn't read,** such as one a faulty own app wrote, or one sealed at
    ///   an epoch whose key the app lacks: [`OutcomeRecord::Held`]. Overwriting it unasked
    ///   could drop a role another own app wrote that this one can't see, and every reader
    ///   that saw it would ignore the write (Paykit wire 5.3, "Seen roles"), so the user
    ///   decides ([`Paykit::repair_activity_record`], `overwrite`): the rebuilt record then
    ///   replaces it, built on the version seen.
    ///
    /// A failure moves no money, and its `failed` is what lets a payment be made again: it
    /// goes with the record's update when the record reads, and alone otherwise.
    async fn outcome_record(
        &mut self,
        entry: &JournalEntry,
        target: &ActivityTarget,
        paid: bool,
        proof: Option<&PaidProof>,
        overwrite: bool,
    ) -> Result<OutcomeRecord> {
        let key = self.activity_key(target)?;
        let held = self.app.shared_record(&key).await?;
        if crate::records::activity_base_changed(held.as_ref())
            || (entry.billing_period.is_some()
                && held
                    .as_ref()
                    .is_some_and(|r| r.content == RecordContent::Deleted))
        {
            return Ok(if paid {
                OutcomeRecord::Held
            } else {
                OutcomeRecord::Unchanged
            });
        }
        // A value only waiting for its scope key opens once the key comes: the outcome waits
        // for it, never offered as a repair over a value that only isn't open yet.
        if held
            .as_ref()
            .is_some_and(|r| crate::hold::awaits_key(&r.content))
        {
            return Err(crate::hold::value_awaits_key());
        }
        // A payment made waits while the record is paused, by the one gate of the outcome
        // ([`crate::divergent::Pause::handed_off_write_waits`]), `done` never going without
        // the record's update; a divergent record the user hasn't confirmed, or one whose
        // kept messages wait to be written, takes no other own write but a stop
        // ([`crate::divergent::Pause::adds_no_stop_waits`]), so a failure's `failed` goes
        // alone, its record left as it is (Paykit spec 6).
        if let Some(current) = &held {
            let pause = Box::pin(self.pause_of(current)).await?;
            if paid && pause.handed_off_write_waits() {
                return Ok(OutcomeRecord::Divergent(key));
            }
            if !paid && pause.adds_no_stop_waits() {
                return Ok(OutcomeRecord::Unchanged);
            }
        }
        let read = match held.as_ref().map(|r| &r.content) {
            None | Some(RecordContent::Deleted) => None,
            Some(_) => match self.read_activity(target, held.as_ref()) {
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
            None => (self.rebuilt_record(entry)?, true),
        };
        changed |= self.place_acceptance(&mut record, entry)?;
        if let Some(proof) = proof {
            if let PaidProof::New(proof) = proof {
                let proof = self.own_copy(proof)?;
                if activity::names_details_not_held(&record, &proof) {
                    let details = entry
                        .selected_period_details(&self.identity())?
                        .ok_or_else(PaykitError::corrupt)?;
                    changed |= activity::place(&mut record, Role::PeriodDetails, details);
                    if activity::names_details_not_held(&record, &proof) {
                        return Ok(OutcomeRecord::Held);
                    }
                }
                changed |= activity::place(&mut record, Role::Proof, proof);
            }
            if entry.billing_period.is_some() {
                // The claim can expire. The payer must then be able to bind the
                // period at this key and find the proof of the payment just recorded.
                let value = record.to_json()?;
                let bound = crate::records::bind_activity(
                    &value,
                    &self.identity(),
                    ActivitySide::Payer,
                    None,
                    &key,
                    |target| self.app.record_key(target).ok(),
                    &|endpoint, proof| self.rail.settlement_id(endpoint, proof),
                );
                if !bound
                    .as_ref()
                    .and_then(|r| r.proof.as_ref())
                    .is_some_and(|stored| stored.hash() == proof.hash())
                {
                    return Ok(OutcomeRecord::Held);
                }
                if !changed && matches!(proof, PaidProof::Original(_)) {
                    return Ok(OutcomeRecord::Compared(
                        self.app.record_condition(&key).await?,
                    ));
                }
            }
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
    /// a Billing Period's request and period, or a payment without a request's own members
    /// (Paykit wire 5.3). A payment of a request that keeps no request is a corrupt store
    /// ([`JournalEntry::check`] refuses it).
    pub(crate) fn rebuilt_record(&self, entry: &JournalEntry) -> Result<ActivityRecord> {
        let own = self.identity();
        let mut record = activity::empty(ActivitySide::Payer, entry.payee);
        match entry.request_hash()? {
            // A period record holds no request: it names its request and Billing Period
            // (Paykit wire 5.3).
            Some(hash) if entry.billing_period.is_some() => {
                record.request_hash = Some(hash);
                record.billing_period = entry.period()?;
            }
            Some(hash) => {
                let Some(kept) = &entry.request else {
                    return Err(PaykitError::corrupt());
                };
                let request = kept.stored(&own)?;
                if request.hash() != hash || request.sender() != entry.payee {
                    return Err(PaykitError::corrupt());
                }
                record.manual_only = entry.user_started;
                record.allowance_id = entry
                    .allowance
                    .as_ref()
                    .map(crate::executor::JournalAllowance::id)
                    .transpose()?;
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
        Ok(record)
    }

    /// The claim's step and this attempt's outcome, read from the Dispatch; `None` when the
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

    /// Fields and references of the original payer proof (Paykit wire 4.10).
    fn payment_proof_fields(&self, entry: &JournalEntry) -> Result<(PaymentProof, Vec<[u8; 32]>)> {
        let request_hash = entry.request_hash()?;
        let proof = PaymentProof {
            attempt_id: entry.attempt()?,
            billing_period: entry.period()?,
            payment_endpoint_identifier: entry.destination.endpoint_id()?,
            payment_app_id: entry.destination.app_id,
            list_version: entry.destination.list_version,
            amount: request_hash
                .is_none()
                .then(|| entry.amount.amount())
                .transpose()?,
            allowance_id: entry
                .allowance
                .as_ref()
                .map(crate::executor::JournalAllowance::id)
                .transpose()?,
            label: entry.label()?,
            payment_reference: None,
            proof: entry.rail_proof.clone().ok_or_else(PaykitError::corrupt)?,
        };
        let mut refs: Vec<[u8; 32]> = request_hash.into_iter().collect();
        refs.extend(
            entry
                .selected_period_details(&self.identity())?
                .map(|signed| signed.hash()),
        );
        Ok((proof, refs))
    }

    /// Match the signed payer proof of a finished attempt to its protected journal.
    pub(crate) fn journal_matches_paid_proof(
        &self,
        entry: &JournalEntry,
        original: &StoredMessage,
    ) -> Result<bool> {
        if entry.rail_proof.is_none()
            || entry.proof_msg_id.as_ref() != Some(&original.message().msg_id)
        {
            return Ok(false);
        }
        let (proof, refs) = self.payment_proof_fields(entry)?;
        Ok(matches_paid_proof(
            original,
            self.identity(),
            self.app_id(),
            entry.payee,
            &refs,
            &proof,
        ))
    }

    /// The proof (Paykit spec 7.2 step 4, Paykit wire 4.10), signed for the outcome's batch.
    async fn outcome_proof(
        &self,
        entry: &JournalEntry,
        target: &ActivityTarget,
    ) -> Result<Option<PaidProof>> {
        let (proof, refs) = self.payment_proof_fields(entry)?;
        if entry.billing_period.is_some() {
            if let Some(record) = self.held_activity(target).await? {
                if let Some(original) = record.proof {
                    let matches = matches_paid_proof(
                        &original,
                        self.identity(),
                        self.app_id(),
                        entry.payee,
                        &refs,
                        &proof,
                    );
                    return Ok(matches.then(|| PaidProof::Original(Box::new(original))));
                }
            }
        }
        let body = proof.to_body(refs.len())?;
        Ok(Some(PaidProof::New(Box::new(self.sign(
            entry.payee,
            Kind::PaymentProof,
            refs,
            body,
        )?))))
    }

    /// Finish the payments a crash, a lost answer or a pending payment method left in the
    /// journal (Paykit spec 7.2): before money could move, an own attempt still open is
    /// written `failed`; once paying, the method's history decides, and a handoff is never
    /// carried out again unless the method is idempotent on the attempt ID; a restored entry
    /// is resolved only from a history that survives a restore. A repeat on an idempotent
    /// method passes every check a first handoff does.
    ///
    /// **Journal pruning** (Paykit spec 7.1, 7.2; Dispatch spec 6.3). After every payment's own
    /// work, a resume drops the `Finished` and `Abandoned` entries past their claim's window:
    /// one with a deadline from that deadline plus 30 days ([`RECONCILIATION_MS`]), the time
    /// the Dispatch keeps the claim's tombstone, and one without from 400 days after it was
    /// journaled ([`MAX_CLAIM_AGE_MS`]). A bound passes only once it lies before the earlier
    /// of the highest service time verified and the device clock plus 24 hours, so no
    /// far-future signed time passes one early, and without a verified service time nothing
    /// is dropped. What a later reconcile, a restore's included, needs is never dropped: an
    /// entry still open or [`Stage::AcceptanceOwed`], one whose handoff the payment method
    /// reported failed while its history may still show it paid, and one paid after `failed`
    /// was written, waiting for its receipt. A dropped payment leaves [`Paykit::payments`],
    /// and its acknowledged notices go with it; the payer's activity record, not the
    /// journal, shows it and stops a second one (Paykit spec 6, 7.2, "The record shows it
    /// paid").
    ///
    /// **Every entry is visited.** Before any entry's step, the method's history is asked
    /// about every payment this app left at [`Stage::Paying`] that is neither restored nor
    /// offline, which needs no Dispatch, and one it reports made is journaled with its proof; a
    /// restored or offline entry does any lookup in its own step. Then each entry's work goes on its own: a failure of one entry's step short of
    /// a wallet-wide stop leaves that entry for the next resume, reported as its outcome,
    /// [`PayOutcome::PaidOutcomeUnwritten`] when the method reported it paid and
    /// [`PayOutcome::Pending`] otherwise, and the next entry goes on. Such a failure that is
    /// a wait, which may pass by itself, such as the network, the Dispatch's answer that a
    /// retry may pass, this SDK's own [`Refusal::NotNow`], a method that can't tell now, or
    /// [`Refusal::IdentityUnavailable`] when a repeat is due and the own `identity.json`
    /// isn't confirmed in the last 60 s, says nothing more, and every outcome reached is in
    /// the result.
    ///
    /// # Errors
    ///
    /// A wallet-wide stop, at which the resume ends at once: an alarm, a store failure, the
    /// Dispatch client's word that the app stopped, left or is gone, holds a recovery notice
    /// or has a restore question open, or the Dispatch's answer that the app is no longer
    /// current, that its certificate expired, or that the account awaits a recovery. Once
    /// every entry is visited and the outbox flushed, the first failure of an entry's step
    /// that is no wait, which a later resume would meet again until the user or the host
    /// acts, as it is, while its entry is still open or still owes its acceptance; nothing
    /// is pruned then. The result is computed once, when the call ends, from the journal
    /// as the call found it and as it leaves it: every entry still open is reported in its
    /// open form, and every entry over whose outcome no call has returned yet is reported
    /// exactly once, whichever step, kept batch or earlier call ended it. That one is
    /// [`PayOutcome::Paid`] with the method's proof, [`PayOutcome::Failed`] only where the
    /// journal holds that no payment was made (the method's word, this app's own failure
    /// written before the method's call, the method's refusal of a first call, or the
    /// user's), and [`PayOutcome::Resolved`]
    /// otherwise. An entry whose outcome a call returned already, this one or a payment
    /// call or [`Paykit::resolve_attempt`], is reported again only when its payment was
    /// found made after all (Paykit spec 7.2 steps 3 and 4). Each recheck lookup is judged as
    /// an entry's step is: one that isn't a wait is returned while the recheck still asks
    /// about its attempt. The client's and the rail's errors from the work before and after
    /// the entries (the first read, the first flush, the ledger and pruning), as they are. A
    /// call that returns an error carries no outcome: the next call that returns reports
    /// the entries it ended, and [`Paykit::payments`] lists every payment with
    /// [`crate::PaymentView::paid_by_method`].
    pub async fn resume_payments(&mut self) -> Result<Vec<PayOutcome>> {
        let _delivery = self.delivery_operation();
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.resume_payments_inner()).await
    }

    async fn resume_payments_inner(&mut self) -> Result<Vec<PayOutcome>> {
        // The journal as the call finds it, before any read or flush can move an entry: what
        // the call returns is computed against it ([`resume_outcomes`]).
        let before = EntryFacts::of(&self.journal_with_facts().await?);
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        // While a wider certificate's scope keys are missing, the resume goes on only on the
        // payer's side it can read: with the `paykit_pay` key held and the payer's records and
        // claims read as before, its writes are built on versions it holds. Otherwise it
        // asks the method's history only, journals what it says, and writes nothing to the
        // Dispatch, as without `paykit_pay`, until the keys come.
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
        // so it goes while the payer's side waits too. Its held messages' windows are judged
        // on the service time verified so far (Paykit spec 8.1).
        Box::pin(self.note_service_time()).await?;
        self.send_outbox().await?;
        // Cancellations kept and not yet placed go into their records (Paykit spec 8.1),
        // which they build on: not while the payer's side waits.
        if !payer_waits {
            self.place_kept_cancellations().await?;
        }
        // The method's history before any entry, which needs no dispatch: a payment it reports
        // made is journaled with its proof before anything an entry's step meets can end the
        // resume. It comes after the flush, so a kept batch's journal writes are in place.
        self.journal_method_answers(payer_waits).await?;
        // Each entry's work stands alone. A wallet-wide stop ([`stops_resume`]) ends the
        // resume and is returned. Any other failure of one entry's step leaves that entry for
        // the next resume, told for it, and the next entry goes on, so a payment the method
        // reports made behind it is still resolved. Each failure that isn't a wait
        // ([`entry_waits`]) is kept with its claim, and judged once the outbox is flushed.
        //
        // An entry is read from the journal again at its turn ([`Paykit::at_its_turn`]): an
        // earlier entry's write may have sent a batch kept for this one, whose journal write
        // replaced what was listed. What the method said of it is laid over it at that read
        // ([`JournalEntry::with_facts`]), so no such write takes the method's word back.
        let mut turns = Turns::default();
        let mut failures = Vec::new();
        for listed in self.journal().await? {
            let Some(mut entry) = self.at_its_turn(listed).await? else {
                continue;
            };
            let attempt = entry.attempt().ok().map(|id| (id, entry.obligation));
            // An outcome written whose acceptance is still owed: queued and placed now.
            // One that can't be placed now stays owed for the next resume.
            if entry.stage == Stage::AcceptanceOwed {
                if !payer_waits {
                    if let Err(error) = self.finish_owed_acceptance(&mut entry).await {
                        if stops_resume(&error) {
                            return Err(error);
                        }
                        if judged_at_resume(&error) && !entry_waits(&error) {
                            failures.push(Pending {
                                error,
                                attempt,
                                work: Work::Acceptance,
                            });
                        }
                    }
                }
            } else {
                let mut told = Vec::new();
                let step = Box::pin(self.resume_entry(entry, payer_waits, &mut told)).await;
                told.into_iter().for_each(|outcome| turns.tell(outcome));
                if let Err(error) = step {
                    if stops_resume(&error) {
                        return Err(error);
                    }
                    if !entry_waits(&error) {
                        failures.push(Pending {
                            error,
                            attempt,
                            work: Work::Step,
                        });
                    }
                }
            }
            // A step can leave the client holding a stop while it returns an error of its
            // own, such as a recovery notice a signed answer to a claim read committed: the
            // resume ends there too, before the next entry is looked up.
            if let Some(stop) = self.held_stop() {
                return Err(stop);
            }
        }
        if self.holds("paykit_pay") && !payer_waits {
            let (rechecked, lookups) = self.recheck_failed().await?;
            rechecked
                .into_iter()
                .for_each(|outcome| turns.tell(outcome));
            failures.extend(lookups);
            Box::pin(self.record_ledger_outcomes()).await?;
        }
        // After every payment's own work: what can't be delivered now stays queued, but a
        // wallet-wide stop met on the flush is returned. Pruning comes last, so after a stop
        // every entry this call finished is still listed by `payments`.
        if let Err(error) = self.send_outbox().await {
            let error = PaykitError::from(error);
            if stops_resume(&error) {
                return Err(error);
            }
        }
        // What the call returns: one function of the journal before and after it.
        let after = EntryFacts::of(&self.journal_with_facts().await?);
        let out = resume_outcomes(&after, &turns);
        // A failure stands only while what it failed at is still to do: a step's while its
        // entry is open, an owed acceptance's while the entry still owes it, a recheck's
        // lookup while the recheck still asks. A later resume meets it again. One whose work
        // is done by now is past, and one whose entry can't be read back stands. A call that
        // returns an error carries no outcome, and its entries ended are told by the next
        // call that returns one ([`AttemptFacts::told`]); nothing is pruned then.
        for failure in failures {
            let entry = failure
                .attempt
                .and_then(|(attempt_id, _)| after.iter().find(|now| now.attempt_id == attempt_id));
            let past = entry.is_some_and(|entry| match failure.work {
                Work::Step => !entry.open(),
                Work::Acceptance => matches!(entry.stage, Stage::Finished | Stage::Abandoned),
                Work::Recheck => !entry.ask_again,
            });
            if !past {
                return Err(failure.error);
            }
        }
        // Test builds check the result against the journals it was computed from, in every
        // test that resumes, on the way to `Ok`.
        #[cfg(feature = "test-support")]
        {
            let disagreement = resume_disagreement(&before, &after, &out);
            assert!(
                disagreement.is_none(),
                "resume_payments: {}",
                disagreement.unwrap_or_default()
            );
        }
        // Every entry over is told now, in one commit, just before the call returns: a call
        // that ends before this, by an error or a crash, leaves them for the next call to
        // report ([`resume_outcomes`]).
        let ended: Vec<(AttemptId, bool)> = after
            .iter()
            .filter(|now| !now.open() && !now.told_as_it_stands())
            .map(|now| (now.attempt_id, now.proof))
            .collect();
        self.mark_told(&ended).await?;
        self.prune_journal().await?;
        Ok(out)
    }

    /// The journal entry `listed` names as the journal holds it now, through the method's word
    /// ([`JournalEntry::with_facts`]), `None` once it is gone: no step of a resume acts on an
    /// entry read before the call's last write.
    async fn at_its_turn(&mut self, listed: JournalEntry) -> Result<Option<JournalEntry>> {
        // An entry that names no attempt can't be read back by one: it goes as listed.
        let Ok(attempt_id) = listed.attempt() else {
            return Ok(Some(listed));
        };
        self.journal_entry_on(&attempt_id, &listed.obligation).await
    }

    /// Ask the payment method's history about every entry this app left `Paying`, and journal
    /// what it says ([`Paykit::journal_method_answer`]). It needs no Dispatch and writes only
    /// the journal, so a payment the method reports made is journaled with its proof whatever
    /// the Dispatch answers later in the resume (Paykit spec 7.2). It is the first thing such an
    /// entry's own step does ([`Paykit::resolve_handed_off`], [`Paykit::take_outcome`]), the
    /// acceptance of a payment made released with it, unless the payer's side waits for its
    /// keys (`payer_waits`), when the report releases it later; a restored entry, which reads
    /// its claim first, and an offline one wait for their own step. A lookup the method
    /// can't answer leaves that entry as it is, for its own step to ask again; a wallet-wide
    /// stop, such as a store failure, is returned. What it learns is kept beside the entry
    /// ([`Paykit::learn`]), where no batch kept from before can take it back.
    async fn journal_method_answers(&mut self, payer_waits: bool) -> Result<()> {
        for listed in self.journal().await? {
            // As the journal holds it now: an acceptance released for an earlier entry may
            // have sent a batch kept for this one.
            let Some(mut entry) = self.at_its_turn(listed).await? else {
                continue;
            };
            if entry.stage != Stage::Paying || entry.restored || entry.offline.is_some() {
                continue;
            }
            // A stop the client holds, such as a recovery notice a read installed meanwhile:
            // nothing more is looked up or written.
            if let Some(stop) = self.held_stop() {
                return Err(stop);
            }
            if let Err(error) = self.journal_method_answer(&mut entry).await {
                if stops_resume(&error) {
                    return Err(error);
                }
                continue;
            }
            // As the entry's own step does once the proof is journaled: its acceptance goes
            // (Paykit spec 7.2 step 2); one that can't be queued now is left to the report.
            // Not while the payer's side waits for its keys: the resume writes nothing to the
            // Dispatch then, and a new acceptance goes once that wait ends ([`KeysAwaited`]).
            if entry.stage == Stage::RailPaid && !payer_waits {
                if let Err(error) = self.release_acceptance(&mut entry, None).await {
                    unless_stop(error)?;
                }
            }
        }
        Ok(())
    }

    /// The wallet-wide stop the Dispatch client holds now, if any: a recovery notice, an alarm,
    /// an app that left, or an open restore question (Dispatch spec 3.6, 12.3, 12.4).
    fn held_stop(&self) -> Option<PaykitError> {
        if let Some(notice) = self.app.recovery_required() {
            return Some(held_back(ClientRefusal::RecoveryRequired {
                generation: notice.generation,
            }));
        }
        self.app
            .may_attempt(false)
            .err()
            .map(held_back)
            .filter(stops_resume)
    }

    /// One open journal entry's resume (Paykit spec 7.2, 7.3). What its steps tell goes to
    /// `out`, of which the resume keeps only what the journal can't hold ([`Turns::tell`]).
    async fn resume_entry(
        &mut self,
        mut entry: JournalEntry,
        payer_waits: bool,
        out: &mut Vec<PayOutcome>,
    ) -> Result<()> {
        let held = !self.holds("paykit_pay") || payer_waits;
        if !held {
            // Under `paykit_pay` again, the payment is held without it no more: that notice
            // goes, so a later certificate without `paykit_pay` tells the host again.
            self.drop_notice(NoticeKind::HeldWithoutPay, &entry).await?;
        }
        if !entry.open() {
            return Ok(());
        }
        let attempt_id = entry.attempt()?;
        // Without `paykit_pay` the app reads and writes no claim and no payer record (the
        // service refuses both, `403 permission_missing`), and makes no payment
        // ([`Paykit::final_payment_check`]): every open entry waits for an app holding
        // it, its proof kept if it has one, and the other entries go on.
        if held {
            out.push(self.held_without_pay(&mut entry).await?);
            return Ok(());
        }
        match entry.stage {
            Stage::Recorded | Stage::Paying if entry.offline.is_some() => {
                if entry.stage == Stage::Recorded && !entry.restored {
                    self.set_stage(&mut entry, Stage::Abandoned).await?;
                } else {
                    out.push(self.offline_method_outcome(&mut entry).await?);
                }
            }
            Stage::Recorded | Stage::Attempted | Stage::Paying if entry.restored => {
                // Restored, it may have been handed off (Paykit spec 7.2): the backup
                // can't tell.
                match self.attempt_state(&entry).await? {
                    Some((step, Outcome::Open | Outcome::Unknown)) => {
                        entry.step = Some(step);
                        entry.stage = Stage::Paying;
                        out.extend(self.resolve_handed_off(entry).await?);
                    }
                    Some((_, outcome)) => self.finish_resolved(&mut entry, outcome).await?,
                    // A claim without the attempt, or no claim at all, proves nothing
                    // either: after a provider loss, once the claim is gone past its
                    // window, or when the backup was taken before its batch reached the
                    // Dispatch, "absence remains insufficient" (Paykit spec 8.4, Late
                    // recording). The attempt is open, its outcome unknown, and the
                    // method's history decides; without it, "the handoff stays open as
                    // potentially paid. Only a receipt or the user can resolve it"
                    // (Paykit spec 7.2): it stays in the doubt the user sees, and the
                    // user resolves it ([`Paykit::resolve_attempt`]).
                    None => {
                        entry.step = None;
                        entry.stage = Stage::Paying;
                        out.extend(self.resolve_handed_off(entry).await?);
                    }
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
                        reason: NO_PAYMENT_STARTED.into(),
                    });
                }
                // Known unsubmitted: this app never called the method.
                Some((_, outcome)) => self.finish_resolved(&mut entry, outcome).await?,
                None => self.set_stage(&mut entry, Stage::Abandoned).await?,
            },
            Stage::Paying => {
                out.extend(self.resolve_handed_off(entry).await?);
            }
            Stage::RailPaid | Stage::RailFailed => {
                out.push(self.resume_report(&mut entry).await?);
            }
            Stage::AcceptanceOwed | Stage::Finished | Stage::Abandoned => {}
        }
        Ok(())
    }

    /// An own attempt the claim shows resolved already, by the user in another app or by
    /// evidence. The acceptance goes only for a payment this app may have handed off
    /// ([`JournalEntry::may_have_handed_off`]): `done` then says it was made, the user's word
    /// standing where the method's history would, so an acceptance owed is queued and placed
    /// ([`Paykit::finish_owed_acceptance`], Paykit spec 7.2 step 2), one queued earlier and
    /// not yet placed included; one that can't be now leaves the entry for the next resume.
    /// An entry known unsubmitted, or `failed`, finishes with none: a payee never sees an
    /// acceptance of a payment that never ran.
    async fn finish_resolved(&mut self, entry: &mut JournalEntry, outcome: Outcome) -> Result<()> {
        if entry.may_have_handed_off()
            && outcome == Outcome::Done
            && entry.may_place_acceptance()?
        {
            if let Err(error) = self.finish_owed_acceptance(entry).await {
                // The resume's loop ends at a stop; anything else leaves the acceptance owed
                // for the next resume, best effort.
                if stops_resume(&error) {
                    return Err(error);
                }
            }
            return Ok(());
        }
        self.set_stage(entry, Stage::Finished).await
    }

    /// An open entry under a certificate without `paykit_pay`, which reads and writes no claim and
    /// no payer record: a handoff that may have been made asks the method's history alone,
    /// so a payment made is journaled with its proof (`RailPaid`) and one that failed as such
    /// (`RailFailed`), each for an app holding `paykit_pay` to report; nothing else is touched. The
    /// host is told why the payment waits ([`NoticeKind::HeldWithoutPay`]).
    async fn held_without_pay(&mut self, entry: &mut JournalEntry) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        self.raise_notice(NoticeKind::HeldWithoutPay, entry).await?;
        if entry.stage == Stage::Paying || entry.restored && entry.may_have_handed_off() {
            self.journal_method_answer(entry).await?;
        }
        Ok(if entry.rail_proof.is_some() {
            PayOutcome::PaidOutcomeUnwritten { attempt_id }
        } else {
            PayOutcome::Pending { attempt_id }
        })
    }

    /// Ask the method's history about `entry`'s handoff and journal what it says, writing
    /// nothing else: a payment made with its proof (`RailPaid`), and one that failed as such
    /// (`RailFailed`), each for the entry's report to write; a payment still running, or none
    /// in the history, leaves the entry as it is. `RailPaid` is the state the proof's journal
    /// write leaves before anything else of the outcome, which a crash may leave too (Paykit
    /// spec 7.2).
    async fn journal_method_answer(&mut self, entry: &mut JournalEntry) -> Result<()> {
        match self.rail.payment(entry.attempt()?).await? {
            Some(RailOutcome::Paid(RailPaid {
                proof, paid_at_ms, ..
            })) => self.learn_paid(entry, proof, paid_at_ms).await,
            Some(RailOutcome::Failed) => self.learn_failed(entry, true).await,
            Some(RailOutcome::Pending) | None => Ok(()),
        }
    }

    /// Keep the method's word that `entry`'s payment was made, with its proof, and the entry
    /// paid with its outcome to write (`RailPaid`, Paykit spec 7.2) ([`Paykit::learn`]).
    pub(crate) async fn learn_paid(
        &mut self,
        entry: &mut JournalEntry,
        proof: serde_json::Map<String, serde_json::Value>,
        paid_at_ms: Option<u64>,
    ) -> Result<()> {
        entry.rail_proof = Some(proof.clone());
        entry.paid_at_ms = paid_at_ms;
        entry.failed_by_method = false;
        entry.stage = Stage::RailPaid;
        self.learn(entry, |facts| {
            // A proof the facts hold already is this attempt's, and stays as it is.
            if facts.proof.is_none() {
                facts.proof = Some(proof);
                facts.paid_at_ms = paid_at_ms;
            }
            facts.ask_again = false;
        })
        .await
    }

    /// Keep the method's word that `entry`'s payment failed, and the entry with that failure
    /// to write (`RailFailed`) ([`Paykit::learn`]). `ask_again` for a payment the method was
    /// called for, whose history the recheck asks again; not for a call it refused, which
    /// submitted nothing (Paykit spec 7.2). A proof the facts hold already stands over it.
    pub(crate) async fn learn_failed(
        &mut self,
        entry: &mut JournalEntry,
        ask_again: bool,
    ) -> Result<()> {
        if entry.rail_proof.is_none() {
            entry.stage = Stage::RailFailed;
        }
        self.learn(entry, |facts| {
            facts.method_failed = true;
            facts.ask_again = ask_again && facts.proof.is_none();
        })
        .await
    }

    /// Drop the journal entries past their claim's reconciliation window (Paykit spec 7.1,
    /// 7.2; Dispatch spec 6.3): `Finished` and `Abandoned` ones, from their deadline plus the
    /// 30 days the Dispatch keeps the claim's tombstone after it ([`RECONCILIATION_MS`]),
    /// or, without a deadline, from 400 days after they were journaled
    /// ([`MAX_CLAIM_AGE_MS`]). A claim pinned to none is kept 400 days after its last entry,
    /// which comes after the journaling, so it may outlive the entry: a finished entry
    /// decides nothing then, since the payer's activity record, not the journal, shows the
    /// payment and stops a second one (Paykit spec 6, 7.2, "The record shows it paid").
    ///
    /// What a later reconcile, a restore's included, needs stays: every entry still open
    /// or owing its acceptance, one whose handoff the payment method reported failed while
    /// its history may still show it paid, and one paid after `failed` was written, whose
    /// receipt the user is told about. A bound passes as [`bound_passed`] says. An entry's
    /// acknowledged notices go with it.
    ///
    /// Three of those have no bound of their own: a payment made after `failed` was written
    /// waits for its receipt for as long as no receipt comes, a handoff the method
    /// reported failed is asked about for as long as the method's history keeps it, without
    /// end for a method whose history is kept for good, and a restored handoff the user
    /// resolved as paid with no claim to settle, whose obligation has no deadline, is the
    /// only thing that stops it being paid again (Paykit spec 7.2, 8.4). Each is one small
    /// entry.
    async fn prune_journal(&mut self) -> Result<()> {
        let service = self.service_time();
        let now = self.now();
        let mut tx = state::Tx::default();
        for entry in self.journal().await? {
            // A user's `done` that no claim settled: with a deadline, the request is refused
            // past it; without one, nothing else stops a second payment.
            if entry.user_paid && entry.deadline.is_none() {
                continue;
            }
            let Some(bound) = prune_bound(
                entry.stage,
                entry.failed_by_method
                    || entry.failed_on_claim
                    || (entry.never_dispatched && entry.stage == Stage::Finished),
                entry.deadline,
                entry.journaled_at_ms,
            ) else {
                continue;
            };
            if bound_passed(bound, service, now) {
                let attempt = entry.attempt()?;
                tx.delete(&key::journal(&attempt))
                    .delete(&key::attempt_facts(&attempt))
                    .delete(&key::offline_batch(&attempt));
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

    /// Ask the payment method's history again about the hands off it reported failed, while
    /// that history keeps them (Paykit spec 7.2, "What a payment method declares"): an owner
    /// that finds a payment it reported failed completed after all sends its proof, and the
    /// payee's receipt corrects the claim. Such a payment is reported as any other, its
    /// activity record and its proof in one batch; a handoff the history no longer keeps
    /// isn't asked about again.
    async fn recheck_failed(&mut self) -> Result<(Vec<PayOutcome>, Vec<Pending>)> {
        let declaration = self.rail.declaration();
        let mut out = Vec::new();
        let mut failures = Vec::new();
        for listed in self.journal().await? {
            // As the journal holds it now: an earlier entry's report may have sent a batch
            // kept for this one.
            let Some(mut entry) = self.at_its_turn(listed).await? else {
                continue;
            };
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
                // The recheck's own flag, which only it clears; the method's word stays.
                entry.failed_by_method = false;
                self.learn(&mut entry, |facts| facts.ask_again = false)
                    .await?;
                continue;
            }
            let attempt_id = entry.attempt()?;
            // Each entry's lookup is judged as the entries' steps are: a wait leaves it for
            // the next recheck, any other failure is kept and judged at the end, and only a
            // wallet-wide stop ends the call; the entries after it are asked all the same.
            let answer = match self.rail.payment(attempt_id).await {
                Ok(answer) => answer,
                Err(error) => {
                    let error = PaykitError::from(error);
                    if stops_resume(&error) {
                        return Err(error);
                    }
                    if !entry_waits(&error) {
                        failures.push(Pending {
                            error,
                            attempt: Some((attempt_id, entry.obligation)),
                            work: Work::Recheck,
                        });
                    }
                    continue;
                }
            };
            let Some(RailOutcome::Paid(RailPaid {
                proof, paid_at_ms, ..
            })) = answer
            else {
                continue;
            };
            // The attempt is `failed` on the claim: no claim entry is written, and the
            // payee's receipt settles it by evidence (Dispatch spec 10.2 row 3), or, once the
            // claim is gone, tells the user it was paid.
            entry.failed_on_claim = entry.offline.is_none() || entry.step.is_some();
            entry.step = None;
            // Opened again, so the outcome's batch is built afresh rather than taken as done.
            self.learn_paid(&mut entry, proof, paid_at_ms).await?;
            out.push(self.report(&mut entry).await?);
        }
        Ok((out, failures))
    }

    /// Resolve an attempt whose payment may have been handed off (Paykit spec 7.2, "What a
    /// payment method declares"): any history record resolves it; an empty history means it
    /// never started only within the history's retention, and for a restored entry only if
    /// the history survives a restore; otherwise an idempotent method is asked again within
    /// its retention, while the claim still shows this app's attempt open, and anything else
    /// stays "may be paid". `None` when the claim shows the attempt resolved already, by the
    /// user or by evidence: the entry is finished, with no outcome of its own.
    pub(crate) async fn resolve_handed_off(
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
            // The attempt's age and orphan window, for the bound on a repeat (Dispatch spec
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
            // The user decided to pay at the first handoff: a request the payee cancelled
            // since, or one this app keeps a stop for, isn't paid again. The repeat is
            // withheld and nothing is written: the first handoff may have paid, so the
            // attempt stays open and "may be paid", never `failed`, until the method's
            // history or a receipt tells (Paykit spec 7.2, 8.1).
            if self.repeat_withheld(&entry).await? {
                return Ok(Some(PayOutcome::Uncertain { attempt_id }));
            }
            let doc = match self.repeat_held(&entry, &claim).await? {
                Ok(doc) => doc,
                Err(held) => return Ok(Some(held)),
            };
            // The retention is judged again by the final payment check and the fence, after
            // the last await and just before the call: the checks awaited the network meanwhile.
            let repeat = Handoff::Repeat {
                idempotency_retention_ms: declaration.idempotency_retention_ms,
                opened_at_ms,
                orphan_after_s,
                longest_completion_ms: declaration.longest_completion_ms,
                read_at,
            };
            return self
                .handoff(entry, doc, Initiator::Automatic, None, repeat, None)
                .await
                .map(Some);
        }
        Ok(Some(PayOutcome::Uncertain { attempt_id }))
    }

    /// Whether a repeat of `entry`'s handoff is withheld because its request may no longer
    /// be paid (Paykit spec 8.1): this app keeps a stop for it, or the request record the
    /// Dispatch holds, read again, shows it cancelled or rejected, or isn't there (a `404`
    /// included, whatever copy this app keeps) or doesn't read as the request, a newer write
    /// this app refused included, so nothing shows it still open; or the request is held
    /// after a restore or a `missed` item until the user confirms it (Paykit spec 7.2,
    /// [`Paykit::confirm_possibly_cancelled`]); or the record holds `manual_only` for an
    /// attempt the user did not start, or the request is a one-time one dated before the
    /// block floor, or the floor can't be read
    /// (`crate::floor`, whatever the record holds: another app may have opened it before it
    /// held the floor): the request is paid only by hand, and a repeat is an automatic
    /// payment (Paykit spec 8.1, 8.3), so the attempt stays open and the user's payment
    /// method's history or a receipt settles it. A payment without a request has nothing a payee cancels.
    async fn repeat_withheld(&mut self, entry: &JournalEntry) -> Result<bool> {
        // The user said an earlier restored handoff of the obligation was paid: no attempt
        // of it is carried out again (Paykit spec 7.2, 8.4).
        if self
            .journal()
            .await?
            .iter()
            .any(|other| other.obligation == entry.obligation && other.user_paid)
        {
            return Ok(true);
        }
        // An earlier-generation handoff is never repeated after a provider loss
        // (Paykit spec 7.3).
        if self.handed_off_before_loss(entry).await? {
            return Ok(true);
        }
        // A repeat is an automatic payment: none while its request record or its list's
        // record is divergent, or paused since it was seen so (Paykit spec 6). The view held
        // is taken before the record is read again, and the one read after it.
        if Box::pin(self.divergent_withholds(entry)).await? {
            return Ok(true);
        }
        // Nor ever to a list destination over a record seen deleted (Paykit spec 6; wire 5):
        // the attempt stays "may be paid" until the method's history or a receipt tells.
        if self
            .list_destination_held(&entry.payee, &entry.destination)
            .await?
        {
            return Ok(true);
        }
        // A Billing Period's request record is the one its payee cancels, beside the period
        // record the outcome writes (Paykit spec 8.2).
        let Some(natural) = entry.request_record()? else {
            return Ok(false);
        };
        let target = ActivityTarget::payer(natural);
        if let Some(period) = entry.period()?.filter(|_| !entry.user_started) {
            return Ok(
                Box::pin(self.automatic_period_conditions(entry, &target, period, None))
                    .await?
                    .is_none(),
            );
        }
        if self.kept_stop(&target).await? {
            return Ok(true);
        }
        Ok(match self.fresh_request(&target).await? {
            Fresh::Held(fresh) => {
                self.fresh_record_withholds(entry, &target, &fresh).await?
                    || match crate::requests::request_of(&fresh) {
                        Some((stored, _)) => self.floor_holds(stored).await?,
                        None => false,
                    }
            }
            Fresh::Missing | Fresh::Unreadable => true,
        })
    }

    /// Whether `fresh`, `entry`'s request record as the Dispatch holds it now, read again
    /// ([`Paykit::fresh_request`]), withholds an automatic payment of it, at a repeat's
    /// withheld check and at the last check before any automatic call
    /// ([`Paykit::automatic_handoff_held`]): the record shows the request cancelled or
    /// rejected, or holds `manual_only` for an attempt the user did not start; the request
    /// is held after a restore or a `missed` item until the user confirms it (Paykit
    /// spec 7.2); or the record, or the list's
    /// record, is divergent or paused since it was seen so, the view read after the record
    /// (Paykit spec 6, 8.1, 8.3).
    async fn fresh_record_withholds(
        &mut self,
        entry: &JournalEntry,
        target: &ActivityTarget,
        fresh: &ActivityRecord,
    ) -> Result<bool> {
        Ok(fresh.lost_to_delete.is_some()
            || cancelled(fresh)
            || rejected(fresh)
            || (fresh.manual_only && !entry.user_started)
            || self.held(&self.activity_key(target)?, fresh).await?
            || Box::pin(self.divergent_withholds(entry)).await?)
    }

    /// Whether `entry`'s request record, a Billing Period's period record, or the
    /// `payment_lists` record of the list its destination came from, blocks an automatic
    /// payment ([`crate::divergent::Pause`]): a divergent request record pauses every period
    /// of the request, and a period record its own (Paykit spec 6, 8.2).
    async fn divergent_withholds(&mut self, entry: &JournalEntry) -> Result<bool> {
        self.divergent_pauses(entry, crate::divergent::Pause::blocks_automatic)
            .await
    }

    async fn divergent_pauses(
        &mut self,
        entry: &JournalEntry,
        rule: fn(crate::divergent::Pause) -> bool,
    ) -> Result<bool> {
        let mut records = Vec::new();
        if let Some(natural) = entry.request_record()? {
            records.push(self.activity_key(&ActivityTarget::payer(natural))?);
        }
        if entry.period()?.is_some() {
            records.push(self.activity_key(&ActivityTarget::payer(entry.natural_key.clone()))?);
        }
        if entry.destination.source()? == DestinationSource::List {
            records.push(
                self.app
                    .record_key(&list_target(&entry.payee, &entry.destination.app_id))?,
            );
        }
        for record in records {
            if rule(Box::pin(self.pause_at(&record)).await?) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The final payment check, on every path that reaches the payment method's call, a first
    /// handoff and a repeat at a resume or a recovery, judged again by the fence just before
    /// the call (Paykit spec 4, 7.2). In order:
    ///
    /// 0. for an automatic first handoff, the deny list, the request record and the block
    ///    floor, each read again and confirmed by a second read
    ///    ([`Paykit::automatic_handoff_held`]): a blocked payee, a record that withholds it
    ///    ([`Paykit::fresh_record_withholds`]: cancelled, rejected, `manual_only`, held after
    ///    a restore, divergent), isn't there or can't be read, a one-time request dated before
    ///    the floor, a read that fails, or a second read that shows another write, holds it
    ///    ([`FinalCheck::Hold`], Paykit spec 8.1, 8.3, `crate::floor`); an automatic repeat
    ///    makes that check after this one;
    /// 1. a Dispatch lookup kept for the payee whose answer has come is taken, so a late
    ///    answer that removes the destination's app is acted on (Dispatch spec 4.3), and one
    ///    the client refuses, such as an equivocation, stops the payment
    ///    ([`FinalCheck::Refused`], Dispatch wire 3.3);
    /// 2. the destination, against the payee's `identity.json` that gives: its carrier
    ///    carries it and hasn't expired, and its app is current, not removed, with the
    ///    certificate the carrier came with;
    /// 3. the payee's copy may still be acted on: read in the last 10 minutes for a
    ///    request's or a list's endpoint, by the Dispatch's rules for a public endpoint;
    /// 4. the own `identity.json` is confirmed in the last 60 s, and lists this app at the
    ///    attempt's service and generation;
    /// 5. for a repeat, the method's idempotency retention hasn't ended, the Allowance it
    ///    was reserved under hasn't expired, and the repeat can complete before its attempt
    ///    is orphaned ([`FinalCheck::Lapsed`]); for a first handoff, the attempt is within
    ///    its pre-handoff budget and the Allowance it was reserved under hasn't expired;
    /// 6. the app may still make the attempt: no alarm or restore question raised meanwhile,
    ///    for an automatic payment the clocks within the tolerance, and its certificate
    ///    holds `paykit_pay`, so an app a renewal reduced moves no money on any path, a repeat at a
    ///    resume or a recovery included ([`FinalCheck::Hold`]: nothing is paid, the attempt
    ///    stays open for an app that holds it).
    ///
    /// Checks 2 to 6 run on the clock after the one await ([`Paykit::final_gates`]). A first
    /// handoff's call follows the journal write of `Paying`; after it, and for a repeat
    /// after this check, an automatic payment makes the check of 0 again
    /// ([`Paykit::automatic_handoff_held`]), and the fence ([`Paykit::payment_fence`]) runs
    /// 1 to 6 again, with no await after them before the call. Anything but
    /// [`FinalCheck::Pay`]: nothing is paid; a copy past its age is looked up again by the
    /// next check, at a resume. Returns the payee's copy the checks judged, for the fence.
    pub(crate) async fn final_payment_check(
        &mut self,
        entry: &JournalEntry,
        payee: PeerIdentity,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        kind: Handoff,
    ) -> Result<(FinalCheck, PeerIdentity)> {
        // No automatic payment to a blocked payee (Paykit spec 8.1), nor of a request its
        // record withholds ([`Paykit::fresh_record_withholds`]: cancelled, rejected,
        // `manual_only`, held after a restore, divergent; Paykit spec 8.1, 8.3), or a one-time
        // request dated before the block floor (`crate::floor`), each read now and confirmed by
        // a second read ([`Paykit::automatic_handoff_held`]), a read that fails holding it
        // too. A first handoff judges them before its attempt's `Paying` commit,
        // so that a held one is known unsubmitted, and again after it ([`Paykit::handoff`]);
        // a repeat, whose entry is `Paying` already, judges them once, after every other
        // await before its call.
        if matches!(kind, Handoff::First { .. }) && self.handoff_held(entry, by, kind).await? {
            return Ok((FinalCheck::Hold, payee));
        }
        let answered = match self.take_kept_lookup(&entry.payee).await? {
            Ok(answered) => answered,
            Err(refusal) => return Ok((FinalCheck::Refused(refusal), payee)),
        };
        let public = entry.destination.source()? == DestinationSource::Public;
        let now = self.now();
        // Paid only while the payee app records kept still use the record selected, judged
        // at this check (Paykit spec 4): otherwise the record is read again.
        let still_used = match entry.kept_carrier(&self.identity())? {
            Carrier::Record(record) => self.seen_still_uses(&entry.payee, &record).await?,
            _ => true,
        };
        let recheck_records = public
            && (answered.is_some()
                || !still_used
                || !self
                    .last_public_read
                    .as_ref()
                    .is_some_and(|read| read.current(&payee, now))
                || !payee_copy_usable(now, &payee, Need::SwitchCopy, by, confirmed));
        let mut payee = answered.unwrap_or(payee);
        if recheck_records {
            match Box::pin(self.check_destination(
                &entry.payee,
                &entry.destination,
                &entry.kept_carrier(&self.identity())?,
                entry.request_hash()?.as_ref(),
                by,
                confirmed,
            ))
            .await?
            {
                Ok(checked) => payee = checked,
                Err(_) => return Ok((FinalCheck::Hold, payee)),
            }
        }
        let check = self.final_gates(entry, &payee, by, confirmed, kind)?;
        Ok((check, payee))
    }

    /// Judge every handoff's local pauses and the shared deny list by its initiator's
    /// rules (Paykit spec 6, 8.1). User consent authorizes only the selected first handoff.
    async fn handoff_held(
        &mut self,
        entry: &JournalEntry,
        by: Initiator,
        kind: Handoff,
    ) -> Result<bool> {
        if self.payment_base_changed(entry).await? {
            return Ok(true);
        }
        // A saved list destination over a record seen deleted is never handed off, a resumed
        // attempt's included; the method's history and a receipt still settle it (Paykit
        // spec 6; wire 5, "Incomplete protected-record contract").
        if self
            .list_destination_held(&entry.payee, &entry.destination)
            .await?
        {
            return Ok(true);
        }
        let crossing_payee = (entry.billing_period.is_some()
            && matches!(kind, Handoff::First { .. }))
        .then_some(entry.payee);
        if entry.billing_period.is_some() {
            let parent =
                ActivityTarget::payer(entry.request_record()?.ok_or_else(PaykitError::corrupt)?);
            let period = ActivityTarget::payer(entry.natural_key.clone());
            for (target, crossing) in [(&parent, crossing_payee), (&period, None)] {
                match Box::pin(self.current_period_activity(target, crossing)).await {
                    Ok(Some(record)) if !payment_ended(&record, crossing) => {}
                    Ok(_) | Err(PaykitError::Refused(_)) => return Ok(true),
                    Err(error) => return Err(error),
                }
            }
        }
        if by == Initiator::Automatic {
            return self
                .automatic_handoff_held(entry, kind, crossing_payee)
                .await;
        }
        // An already accepted first attempt may finish across the seller's cancellation.
        // A deletion that could have hidden a paid proof still holds it (Paykit spec 6, 8.1).
        if entry.billing_period.is_none() && entry.request_hash.is_some() {
            let target = ActivityTarget::payer(entry.natural_key.clone());
            if !matches!(Box::pin(self.fresh_request(&target)).await?, Fresh::Held(record) if record.lost_to_delete.is_none())
            {
                return Ok(true);
            }
        }
        if Box::pin(self.divergent_pauses(entry, crate::divergent::Pause::blocks_by_hand)).await? {
            return Ok(true);
        }
        Ok(self.denied_for_user(&entry.payee).await?
            && !matches!(
                kind,
                Handoff::First {
                    blocked_confirmed: true,
                    ..
                }
            ))
    }

    /// A journal entry binds its payment to this one payer record (Paykit spec 7.2).
    async fn payment_base_changed(&self, entry: &JournalEntry) -> Result<bool> {
        let target = ActivityTarget::payer(entry.natural_key.clone());
        self.activity_base_changed(&self.activity_key(&target)?)
            .await
    }

    /// Whether `entry` is held from an automatic handoff, a first handoff or a repeat, as
    /// the Dispatch holds the identity's state now. Every automatic handoff makes this check,
    /// including after taking a late seller lookup. The final local checks follow the
    /// last read without waiting ([`Paykit::payment_fence`]).
    ///
    /// It reads three things from the Dispatch, one after the other ([`Paykit::shared_collect`]):
    /// the deny list, the request record ([`Paykit::fresh_request_stamped`]) and the block
    /// floor ([`Paykit::floor_read_now`], for a one-time request only), and judges them: the
    /// deny list holds the payee (Paykit spec 8.1); the record withholds the request
    /// ([`Paykit::fresh_record_withholds`]: cancelled, rejected, `manual_only`, held after a
    /// restore, divergent), or isn't there, doesn't read as the request or holds none; or the
    /// floor holds the request ([`Paykit::floor_holds_read`]). Then it reads the three again,
    /// in the same order, and holds unless each shows what its first read did (the deny list
    /// the same entries, each record the same write, [`WriteStamp`]), or the deny list now
    /// holds the payee, or the record isn't there, doesn't read or holds no request. Each read
    /// shows the Dispatch's current state, never a copy this app keeps: the deny list from
    /// `GET /allow`, which the client always fetches, with this app's own block or unblock
    /// that the list doesn't show yet counting at once (Paykit spec 8.1, `judged_deny`): an
    /// unblock whose batch the client still keeps takes the payee out even while
    /// `GET /allow` lists them; the record and the floor from
    /// `read_record_fresh`, a read that shows anything but the Dispatch's current write holding
    /// the handoff.
    ///
    /// For a period started under the host's policy, each pass instead reads the deny list,
    /// subscription and occurrence. Compare the exact applications of both permitted records,
    /// keeping every independent restriction found during either pass.
    ///
    /// Another own app can block the payee, record a cancellation or a rejection, mark the
    /// record `manual_only` or raise the floor while this app awaits anything, between these
    /// reads included. A record's version only grows, whoever writes, a delete included, so an
    /// unchanged version is the same write; the deny list carries no version and is compared
    /// whole. Both reads of the record and of the floor bracket the second read of the deny
    /// list, so each value judged held at that instant. A read that fails, or a record not
    /// there or not readable at either read, holds the handoff. A write that lands after its
    /// own second read is too late for this call: the payment method's call can't be made
    /// conditional on the Dispatch's state, so no read before it sees a write after that read.
    /// After the second collect's last Dispatch read, only this app's own store is awaited
    /// (for a recurring request, the copy of the record just taken; for a payment without a
    /// request, the commit of the deny list read and the widenings it applies), and then
    /// only code with no await runs before the call.
    async fn automatic_handoff_held(
        &mut self,
        entry: &JournalEntry,
        kind: Handoff,
        crossing_payee: Option<[u8; 32]>,
    ) -> Result<bool> {
        let Some(judged) = Box::pin(self.shared_collect(entry, kind, true, crossing_payee)).await?
        else {
            return Ok(true);
        };
        let Some(confirmed) =
            Box::pin(self.shared_collect(entry, kind, false, crossing_payee)).await?
        else {
            return Ok(true);
        };
        if judged == confirmed {
            return Ok(false);
        }
        // A seller's cancellation arrived between the two reads. Confirm the new
        // parent, unchanged occurrence, deny list and independent holds together.
        if judged.deny == confirmed.deny
            && matches!((&judged.period, &confirmed.period),
            (Some(before), Some(after)) if !before[0].crossed_stop && after[0].crossed_stop && before[1] == after[1])
        {
            let latest = Box::pin(self.shared_collect(entry, kind, false, crossing_payee)).await?;
            return Ok(latest.as_ref() != Some(&confirmed));
        }
        Ok(true)
    }

    /// An automatic period payment must still be authorized by both current
    /// records at resume and immediately before the rail call (Paykit spec 8.2, 8.3).
    async fn automatic_period_conditions(
        &mut self,
        entry: &JournalEntry,
        parent: &ActivityTarget,
        period: PeriodRef,
        crossing_payee: Option<[u8; 32]>,
    ) -> Result<Option<Box<[crate::periods::AutomaticPeriodWrite; 2]>>> {
        let occurrence = ActivityTarget::payer(entry.natural_key.clone());
        let funding = entry
            .allowance
            .as_ref()
            .map(crate::executor::JournalAllowance::id)
            .transpose()?;
        let Some(parent) = Box::pin(self.automatic_period_condition(
            parent,
            period,
            true,
            funding.as_ref(),
            crossing_payee,
        ))
        .await?
        else {
            return Ok(None);
        };
        let Some(current) = Box::pin(self.automatic_period_condition(
            &occurrence,
            period,
            false,
            funding.as_ref(),
            None,
        ))
        .await?
        else {
            return Ok(None);
        };
        if Box::pin(self.divergent_withholds(entry)).await? {
            return Ok(None);
        }
        Ok(Some(Box::new([parent, current])))
    }

    /// One read of the identity's state an automatic handoff of `entry` is judged by
    /// ([`Paykit::automatic_handoff_held`]): the deny list, then the request record, then the
    /// block floor for a one-time request, each from the Dispatch now. `None` when a read fails,
    /// the deny list holds the payee, the record isn't there, doesn't read or holds no
    /// request, or, when `judge`, the record withholds the request or the floor holds it.
    /// Without `judge`, nothing more is read from the Dispatch after the last read; only the
    /// app's own store is.
    async fn shared_collect(
        &mut self,
        entry: &JournalEntry,
        kind: Handoff,
        judge: bool,
        crossing_payee: Option<[u8; 32]>,
    ) -> Result<Option<SharedState>> {
        if Box::pin(self.executor_handoff_held(entry, kind)).await? {
            return Ok(None);
        }
        let deny = match self.deny_list_now().await {
            Ok(deny) => deny,
            Err(error) => {
                unless_stop(error)?;
                return Ok(None);
            }
        };
        if deny.contains(&entry.payee) {
            return Ok(None);
        }
        let Some(natural) = entry.request_record()? else {
            return Ok(Some(SharedState {
                deny,
                record: None,
                floor: None,
                period: None,
            }));
        };
        let target = ActivityTarget::payer(natural);
        if let Some(period) = entry.period()?.filter(|_| !entry.user_started) {
            let Some(conditions) =
                Box::pin(self.automatic_period_conditions(entry, &target, period, crossing_payee))
                    .await?
            else {
                return Ok(None);
            };
            return Ok(Some(SharedState {
                deny,
                record: None,
                floor: None,
                period: Some(conditions),
            }));
        }
        let (record, stamp) = match self.fresh_request_stamped(&target).await {
            Ok((Fresh::Held(record), Some(stamp))) => (*record, stamp),
            Ok(_) => return Ok(None),
            Err(error) => {
                unless_stop(error)?;
                return Ok(None);
            }
        };
        let allowance = entry
            .allowance
            .as_ref()
            .map(crate::executor::JournalAllowance::id)
            .transpose()?;
        let hand_repeat =
            entry.user_started && allowance.is_none() && matches!(kind, Handoff::Repeat { .. });
        if !hand_repeat && record.allowance_id != allowance {
            return Ok(None);
        }
        if judge && self.fresh_record_withholds(entry, &target, &record).await? {
            return Ok(None);
        }
        let Some((stored, _)) = crate::requests::request_of(&record) else {
            return Ok(None);
        };
        if !crate::floor::floor_applies(stored) {
            return Ok(Some(SharedState {
                deny,
                record: Some(stamp),
                floor: None,
                period: None,
            }));
        }
        let stored = stored.clone();
        // The shared post-loss hold, judged at both collects by the newest own `identity.json`
        // verified, holds every automatic handoff of a one-time request, a repeat included
        // (Paykit spec 8.1).
        if self.shared_loss_hold().await? {
            return Ok(None);
        }
        let Some(floor) = self.floor_read_now().await? else {
            return Ok(None);
        };
        if judge && self.floor_holds_read(&stored, floor.as_ref()).await? {
            return Ok(None);
        }
        Ok(Some(SharedState {
            deny,
            record: Some(stamp),
            floor: Some(floor.as_ref().map(WriteStamp::of)),
            period: None,
        }))
    }

    /// Recheck the payer's shared state after journaling `Paying` and immediately before
    /// handoff (Paykit spec 7.2, 8.1, 8.3). A first payment to a message-carried
    /// destination can use a late Dispatch confirmation of the exact signed seller identity
    /// already checked. Taking it awaits storage, so read the payer's state again. A
    /// public endpoint needs its app record checked too, and a repeat may already have paid.
    async fn payment_fence(
        &mut self,
        entry: &JournalEntry,
        mut payee: PeerIdentity,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        kind: Handoff,
    ) -> Result<FinalCheck> {
        let mut may_take_answer = matches!(kind, Handoff::First { .. });
        loop {
            // A check that fails here, a payment by hand's fresh reads or the store, is the
            // caller's error, not a hold ([`Paykit::unread_at_the_fence`]). An automatic
            // payment's checks take a read that fails as a hold themselves.
            if self.handoff_held(entry, by, kind).await?
                || Box::pin(self.period_recovery_held(entry)).await?
            {
                return Ok(FinalCheck::Hold);
            }
            // Whether the Dispatch's answer confirms the document checked, and whether it learned
            // anything at all: once finished, the answer carries the status of the newest
            // lookup, which can hide a copy this one read, and a lookup that failed can still
            // carry a copy or a freeze (Pubky Dispatch spec, Section 4.3).
            let ready = self.inspect_ready_lookup(&entry.payee, |answer| {
                (answer.confirms(&payee.doc), answer.learned_nothing())
            });
            let Some((confirmed_document, learned_nothing)) = ready else {
                // No await from these checks through to the call of the payment method.
                return self.final_gates(entry, &payee, by, confirmed, kind);
            };
            if !may_take_answer || entry.destination.source()? == DestinationSource::Public {
                return Ok(FinalCheck::Hold);
            }
            match self.take_kept_lookup(&entry.payee).await? {
                Ok(Some(answer)) => match fence_news(
                    answer.doc.signed() == payee.doc.signed(),
                    payee.switch_copy,
                    Answer {
                        copy: answer.switch_copy,
                        learned_nothing,
                        confirmed: confirmed_document,
                    },
                    by,
                ) {
                    FenceNews::Confirms => {
                        payee = answer;
                        may_take_answer = false;
                    }
                    FenceNews::None => may_take_answer = false,
                    FenceNews::Holds => return Ok(FinalCheck::Hold),
                },
                Ok(None) => return Ok(FinalCheck::Hold),
                Err(refusal) => return Ok(FinalCheck::Refused(refusal)),
            }
        }
    }

    /// Whether a repeat made now completes, at the method's longest, before its attempt is
    /// orphaned (Dispatch spec 10.2): `now + longest_completion_ms < opened_at + orphan_after`,
    /// on the service clock. Service time now is taken late: the latest of the highest
    /// service time verified, the device clock plus the clock tolerance, and the service
    /// time verified at the claim read plus the monotonic time since (`read_at`), which a
    /// device clock set back meanwhile can't shorten. Past it another
    /// own app may soon resolve the attempt, `unknown` or `failed` with the user's
    /// confirmation (rows 5 and 6), while the repeat's payment could still run, so it isn't
    /// made, and the attempt stays "may be paid" as past the idempotency retention.
    ///
    /// The fence judges it again after a repeat's last await ([`Paykit::payment_fence`]): this
    /// check, with the others, is the last before the call, and nothing awaits between them.
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

    /// A first handoff's attempt is still within the pre-handoff budget
    /// ([`crate::PRE_HANDOFF_BUDGET_MS`]): its age since just before it was sent, by the
    /// device's wall and monotonic clocks, with the clock tolerance added, is under the
    /// budget, and the
    /// own certificate hasn't expired by the service time that makes. `orphan_after` is at
    /// least the rail's longest time plus the budget ([`crate::orphan_floor_s`]), so within
    /// it no other own app can resolve the attempt as orphaned before its payment has run
    /// (Dispatch spec 10.2 rows 5 and 6). Past it nothing is submitted: the attempt is written
    /// `failed`, and a payment made after is a new decision.
    ///
    /// Residual: without a read, nothing here can see the owner's own revocation within the
    /// budget. An open attempt whose owner isn't current is orphaned at once, but rows 5 and
    /// 6 need the user's confirmation and flag the next attempt `possibly_duplicate`, which
    /// is never paid automatically (Paykit spec 7.3), and the signer warns before revoking
    /// an app that owns open attempts.
    fn within_budget(&self, answered: &Answered, now: u64) -> std::result::Result<(), Refusal> {
        let age = answered.age_ms(now).saturating_add(CLOCK_TOLERANCE_MS);
        if age >= crate::PRE_HANDOFF_BUDGET_MS {
            return Err(Refusal::HeldForUser(
                crate::HeldForUserReason::AttemptCheck(
                    "Dispatch spec 10.2: the attempt is past its pre-handoff budget",
                ),
            ));
        }
        if self
            .app
            .cert()
            .expired_at(answered.opened_at_ms.saturating_add(age))
        {
            return Err(Refusal::HeldForUser(
                crate::HeldForUserReason::AttemptCheck(
                    "Dispatch spec 3.2: the app's certificate expires within the pre-handoff budget",
                ),
            ));
        }
        Ok(())
    }

    /// The last clock sample before calling the rail, and whether that sample
    /// still authorizes the currently held own identity (Paykit spec 4).
    fn handoff_time(&self) -> (u64, bool) {
        self.app
            .identity_confirmed_at()
            .zip(self.app.identity_confirmation_age_ms())
            .map_or_else(
                || (self.now(), false),
                |(start, age)| (start + age, age < REVALIDATION_MS),
            )
    }

    fn payee_source_current(
        &self,
        carrier: &Carrier,
        payee: &PeerIdentity,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        now: u64,
    ) -> bool {
        (!matches!(carrier, Carrier::Record(_))
            || self
                .last_public_read
                .as_ref()
                .is_some_and(|read| read.current(payee, now)))
            && payee_copy_usable(now, payee, Need::of(carrier), by, confirmed)
    }

    /// Check the independent rate signer's certificate against the destination's current
    /// payee identity. The journal retains the signed message and its certificate.
    fn current_rate_details(
        &self,
        entry: &JournalEntry,
        payee: &PeerIdentity,
    ) -> Result<std::result::Result<Option<StoredMessage>, Refusal>> {
        let selected = entry.selected_rate_details(&self.identity())?;
        if let Some(details) = &selected {
            if let Err(reason) =
                crate::destination::current_rate_source(&payee.doc, details, self.now())
            {
                return Ok(Err(Refusal::Destination(reason)));
            }
        }
        Ok(Ok(selected))
    }

    /// Checks 2 to 6 of the final payment check, with no await, on the clock now and the
    /// payee's copy `payee`, and for a first handoff the pre-handoff budget
    /// ([`Paykit::within_budget`]).
    fn final_gates(
        &self,
        entry: &JournalEntry,
        payee: &PeerIdentity,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        kind: Handoff,
    ) -> Result<FinalCheck> {
        if !self.holds("paykit_pay")
            || entry.allowance.is_some() && !self.holds("paykit_allowance_execute")
        {
            return Ok(FinalCheck::Hold);
        }
        let automatic_first = by == Initiator::Automatic && matches!(kind, Handoff::First { .. });
        if let Err(refusal) = self.transfer_holds(entry, payee, automatic_first)? {
            return Ok(FinalCheck::Refused(refusal));
        }
        // A stop of the app as a whole raised during an await is returned, the entry still
        // `Attempted`, known unsubmitted; a wait holds the payment back.
        let may_attempt = match self.app.may_attempt(by == Initiator::Automatic) {
            Ok(()) => true,
            Err(refusal) => {
                unless_stop(held_back(refusal))?;
                false
            }
        };
        let (now, own_fresh) = self.handoff_time();
        match kind {
            Handoff::Repeat {
                idempotency_retention_ms,
                opened_at_ms,
                orphan_after_s,
                longest_completion_ms,
                read_at,
            } => {
                if !retained(idempotency_retention_ms, entry.journaled_at_ms, now) {
                    return Ok(FinalCheck::Lapsed);
                }
                // An expired Allowance pays nothing more (Paykit spec 8.3): no repeat of a
                // handoff that may have gone, and the attempt stays open as possibly paid,
                // for the method's history or a receipt to settle, never written failed.
                if allowance_expired(entry, now, self.service_time()) {
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
            Handoff::First { answered, .. } => {
                if let Err(refusal) = self.within_budget(&answered, now) {
                    return Ok(FinalCheck::Refused(refusal));
                }
                // An expired Allowance pays nothing more (Paykit spec 8.3): the reservation's
                // window bounds when the attempt was written, this when its first handoff
                // goes. Nothing was submitted, so the attempt is written `failed`; a repeat is
                // stopped above instead, as `Lapsed`, since the first call may have paid.
                if allowance_expired(entry, now, self.service_time()) {
                    return Ok(FinalCheck::Refused(Refusal::NotAutomatic(
                        "the Allowance expired before handoff".into(),
                    )));
                }
                if automatic_first
                    && entry.period()?.is_some_and(|period| {
                        crate::periods::automatic_window_ended(
                            &period,
                            entry.deadline,
                            self.service_time(),
                        )
                    })
                {
                    return Ok(FinalCheck::Refused(Refusal::NotAutomatic(
                        "the Billing Period's automatic window ended before handoff".into(),
                    )));
                }
            }
        }
        let carrier = entry.kept_carrier(&self.identity())?;
        let destination = self
            .destination_valid(
                &entry.payee,
                &entry.destination,
                &carrier,
                entry.request_hash()?.as_ref(),
                &payee.doc,
                now,
            )
            .is_ok();
        // An alarm or a restore question raised during any await holds the payment back
        // (Dispatch spec 10.1, 12.4).
        let clear = may_attempt
            && destination
            && self.payee_source_current(&carrier, payee, by, confirmed, now)
            && own_fresh
            && self.own_identity_authorizes(entry, now).is_ok();
        Ok(if clear {
            FinalCheck::Pay
        } else {
            FinalCheck::Hold
        })
    }

    /// The checks before a repeat, the same as before a first handoff (Paykit spec 7.2 step
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
        let carrier = entry.kept_carrier(&self.identity())?;
        let gate = self
            .pay_gate(entry, &carrier, claim, Initiator::Automatic, None, true)
            .await?;
        match gate {
            Ok(doc) => Ok(Ok(doc)),
            Err(held) => repeat_hold(held, attempt_id).map(Err),
        }
    }

    /// The gate before money moves, the same for a first handoff and a repeat (Paykit spec
    /// 4, 7.2 step 1). In order:
    ///
    /// 1. the app may make an attempt now: no alarm, not left, no restore question, not in
    ///    reconcile mode, no payer pass owed since a restore ([`Paykit::payer_pass_owed`])
    ///    and, for an automatic payment, the clocks within the tolerance (Dispatch spec 10.1,
    ///    12.4; Paykit spec 7.3);
    /// 2. `claim`, the view of a checked proof or of a claim read, shows the attempt open,
    ///    this app's and current (Paykit spec 7.2, "Only a verified answer lets money move");
    /// 3. the destination passes its checks against its carrier and the payee's
    ///    `identity.json`, for the initiator (Paykit spec 4, 7.2; Dispatch spec 4.3);
    /// 4. the own `identity.json` is confirmed in the last 60 s, read again first with
    ///    `refresh`, judged on the clock after the read (Paykit spec 4);
    /// 5. that copy lists this app, not removed, with the certificate it holds counting now
    ///    (Paykit spec 4, Dispatch spec 3.2);
    /// 6. that copy names the generation the attempt was written at, and the service whose
    ///    key signed the attempt's transition proof (Paykit spec 4, "Fresh before paying";
    ///    Dispatch spec 12.5, 12.6).
    ///
    /// `Err` inside holds the first refusal, as [`Held::Client`] for 1, [`Held::OwnIdentity`]
    /// for 4, and [`Refusal::NotCurrent`] for 5 and 6.
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
            return Ok(Err(Held::Client(refusal)));
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
        if let Err(refusal) = self.own_identity_confirmed(refresh).await? {
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
            self.now(),
        ) {
            return Ok(Err(Held::Check(refusal)));
        }
        if let Err(refusal) = self.current_rate_details(entry, &doc)? {
            return Ok(Err(Held::Check(refusal)));
        }
        if let Err(why) = self.own_identity_authorizes(entry, self.now()) {
            return Ok(Err(Held::Check(Refusal::NotCurrent(why))));
        }
        Ok(Ok(doc))
    }

    fn own_identity_lists_app(&self, now: u64) -> std::result::Result<(), String> {
        let doc = self.app.own_identity();
        let me = self.app_id();
        let cert = self.app.cert().hash();
        if doc.lists_removed(&me)
            || doc.listed_cert(&me).is_none()
            || !doc.counts_at(&me, &cert, now)
        {
            return Err(
                "Paykit spec 4: the own identity.json doesn't list this app with its certificate"
                    .into(),
            );
        }
        Ok(())
    }

    /// Checks 5 and 6 of [`Paykit::pay_gate`], on the own `identity.json` held.
    fn own_identity_authorizes(
        &self,
        entry: &JournalEntry,
        now: u64,
    ) -> std::result::Result<(), String> {
        self.own_identity_lists_app(now)?;
        let doc = self.app.own_identity();
        let Some(signed) = &entry.attempt_proof else {
            return Err("Paykit spec 4: the attempt's proof isn't kept".into());
        };
        let generation = TransitionProof::peek(signed)
            .map_err(|_| "Paykit spec 4: the attempt's proof doesn't read".to_owned())?
            .envelope()
            .generation;
        if generation != doc.generation() {
            return Err(
                "Dispatch spec 12.5: the identity names another generation than the attempt's"
                    .into(),
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

/// Whether `error` says the own Dispatch doesn't answer now: the request or its answer was
/// lost, the Dispatch answered `503` (`unavailable`, or `staged` while its account is set
/// up), or an answer with a `5xx` status came without the service's signature, as from a
/// proxy in front of it; or the account may have been erased, and the app's wait before it
/// takes it as gone goes on (Dispatch spec 12.7).
pub(crate) fn dispatch_unavailable(error: &AppError) -> bool {
    match error.unkept() {
        AppError::Transport(_) | AppError::NotAnsweringAsAccount { .. } => true,
        AppError::Switch(pubky_dispatch_client::error::ClientError::Transport(_)) => true,
        AppError::Switch(pubky_dispatch_client::error::ClientError::BadServiceSignature {
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

/// The write of a shared record a read showed: its version, which only grows across the
/// account whoever writes, a delete included, and what the client marks on
/// it without a new write: a write refused, a removed app's write nothing dates, a view that
/// conflicts. Two reads that show the same stamp show the same write, as the client holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriteStamp {
    version: RecordVersion,
    flagged: bool,
    doubtful: bool,
    divergent: bool,
}

impl WriteStamp {
    /// The stamp of `record` as read.
    pub(crate) fn of(record: &SharedRecord) -> Self {
        Self {
            version: record.version,
            flagged: record.flagged,
            doubtful: record.doubtful,
            divergent: record.divergent,
        }
    }
}

/// What one read of the identity's state for an automatic handoff showed
/// ([`Paykit::shared_collect`]): the deny list, the request record's write, and the floor
/// record's (`None` inside for no record), each `None` when not read.
#[derive(Debug, PartialEq, Eq)]
struct SharedState {
    deny: Vec<[u8; 32]>,
    record: Option<WriteStamp>,
    floor: Option<Option<WriteStamp>>,
    period: Option<Box<[crate::periods::AutomaticPeriodWrite; 2]>>,
}

#[cfg(test)]
mod tests {
    use pubky_dispatch_client::error::{ClientError, StoreError, TransportError};

    use super::*;

    /// Every state the journal can hold for an entry, as a resume's result reads it: a stage
    /// that needs the method's proof only with it (`RailPaid`, `AcceptanceOwed`), a proof
    /// with and without its message, no payment started only for an entry finished, or
    /// failed by a refused call, that the method never answered, the recheck asking only about the method's failure, and
    /// told only for an entry over.
    fn every_entry(attempt_id: AttemptId) -> Vec<EntryFacts> {
        use crate::journal::UserResolution;
        let mut all = Vec::new();
        for stage in [
            Stage::Recorded,
            Stage::Attempted,
            Stage::Paying,
            Stage::RailPaid,
            Stage::RailFailed,
            Stage::AcceptanceOwed,
            Stage::Finished,
            Stage::Abandoned,
        ] {
            for proof_msg_id in [None, Some(None), Some(Some("m"))] {
                let proof = proof_msg_id.is_some();
                for method_failed in [false, true] {
                    for ask_again in [false, true] {
                        for failed_before_call in [false, true] {
                            for user_resolved in [
                                None,
                                Some(UserResolution::Done),
                                Some(UserResolution::Unknown),
                                Some(UserResolution::Failed),
                            ] {
                                for (told, told_paid) in
                                    [(false, false), (true, false), (true, true)]
                                {
                                    let open = !matches!(
                                        stage,
                                        Stage::AcceptanceOwed | Stage::Finished | Stage::Abandoned
                                    );
                                    // The states the journal refuses to hold, the
                                    // entry's own failure mark being the recheck's flag
                                    // ([`crate::journal::members_hold`]); facts no write
                                    // leaves ([`AttemptFacts::check`]); no payment started
                                    // beside the method's word, which the overlay clears;
                                    // told paid only with a proof; told and open only when
                                    // the recheck opened it again with a proof found after.
                                    if !crate::journal::members_hold(
                                        &crate::journal::EntryMembers {
                                            stage: Some(stage),
                                            proof,
                                            failed_by_method: ask_again,
                                            failed_before_call,
                                            user_resolved,
                                            ..Default::default()
                                        },
                                    ) || ask_again && (!method_failed || proof)
                                        || failed_before_call && method_failed
                                        || told_paid && !proof
                                        || told && open && (!proof || told_paid)
                                    {
                                        continue;
                                    }
                                    all.push(EntryFacts {
                                        attempt_id,
                                        stage,
                                        proof,
                                        proof_msg_id: proof_msg_id.flatten().map(Into::into),
                                        method_failed,
                                        ask_again,
                                        failed_before_call,
                                        user_resolved,
                                        told,
                                        told_paid,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        all
    }

    #[test]
    fn test_resume_outcomes_agree_with_the_journals_for_every_pair_of_states() {
        let attempt_id = AttemptId::from_bytes([7; 16]);
        let states = every_entry(attempt_id);
        let told = [
            None,
            Some(PayOutcome::Uncertain { attempt_id }),
            Some(PayOutcome::PaidUnreported { attempt_id }),
            Some(PayOutcome::RecordHeld { attempt_id }),
            Some(PayOutcome::RecordDivergent {
                attempt_id,
                record: [1; 32],
            }),
            // What a step tells and the result never returns as told.
            Some(PayOutcome::Pending { attempt_id }),
            Some(PayOutcome::Failed {
                attempt_id,
                reason: METHOD_FAILED.into(),
            }),
        ];
        let mut checked = 0;
        for was in states.iter().map(Some).chain([None]) {
            for now in &states {
                // No call takes a proof back, nor the method's failure, nor told
                // ([`AttemptFacts::grows_to`]).
                if was.is_some_and(|was| {
                    was.proof && !now.proof
                        || was.method_failed && !now.method_failed
                        || was.told && !now.told
                }) {
                    continue;
                }
                for told in &told {
                    let mut turns = Turns::default();
                    told.iter().for_each(|outcome| turns.tell(outcome.clone()));
                    let before: Vec<_> = was.cloned().into_iter().collect();
                    let after = [now.clone()];
                    let out = resume_outcomes(&after, &turns);
                    assert_eq!(
                        resume_disagreement(&before, &after, &out),
                        None,
                        "{was:?} -> {now:?}, told {told:?}: {out:?}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 10_000, "{checked}");
    }

    #[test]
    fn test_resume_outcomes_keep_what_a_step_told_of_an_open_entry() {
        let attempt_id = AttemptId::from_bytes([7; 16]);
        let open = |proof: bool| EntryFacts {
            attempt_id,
            stage: if proof {
                Stage::RailPaid
            } else {
                Stage::Paying
            },
            proof,
            proof_msg_id: None,
            method_failed: false,
            ask_again: false,
            failed_before_call: false,
            user_resolved: None,
            told: false,
            told_paid: false,
        };
        for (proof, told) in [
            (false, PayOutcome::Uncertain { attempt_id }),
            (true, PayOutcome::PaidUnreported { attempt_id }),
            (true, PayOutcome::RecordHeld { attempt_id }),
            (
                true,
                PayOutcome::RecordDivergent {
                    attempt_id,
                    record: [1; 32],
                },
            ),
        ] {
            let mut turns = Turns::default();
            turns.tell(told.clone());
            let entry = [open(proof)];
            assert_eq!(resume_outcomes(&entry, &turns), vec![told]);
        }
    }

    #[test]
    fn test_resume_outcomes_report_a_failure_only_on_evidence() {
        use crate::journal::UserResolution;
        let attempt_id = AttemptId::from_bytes([7; 16]);
        let entry = |stage| EntryFacts {
            attempt_id,
            stage,
            proof: false,
            proof_msg_id: None,
            method_failed: false,
            ask_again: false,
            failed_before_call: false,
            user_resolved: None,
            told: false,
            told_paid: false,
        };
        let one = |was: &EntryFacts, now: &EntryFacts| {
            let _ = was;
            let out = resume_outcomes(std::slice::from_ref(now), &Turns::default());
            assert!(out.len() <= 1, "{out:?}");
            out.into_iter().next()
        };
        let resolved = Some(PayOutcome::Resolved { attempt_id });
        let failed = |reason: &str| {
            Some(PayOutcome::Failed {
                attempt_id,
                reason: reason.into(),
            })
        };
        // The user's `done` or `unknown` on the claim, applied by a batch sent at another
        // entry's turn: over without a proof, and never a failure.
        for resolution in [UserResolution::Done, UserResolution::Unknown] {
            let now = EntryFacts {
                user_resolved: Some(resolution),
                ..entry(Stage::Finished)
            };
            assert_eq!(one(&entry(Stage::Attempted), &now), resolved);
        }
        // The user's `failed` is the user's.
        let now = EntryFacts {
            user_resolved: Some(UserResolution::Failed),
            ..entry(Stage::Finished)
        };
        assert_eq!(one(&entry(Stage::Paying), &now), failed(USER_FAILED));
        // Another own app resolved it on the claim: this journal holds no mark, so nothing
        // says no payment was made.
        for was in [Stage::Recorded, Stage::Attempted, Stage::Paying] {
            assert_eq!(one(&entry(was), &entry(Stage::Finished)), resolved);
        }
        // This app's own `failed` before the method's call, once its write applied.
        let now = EntryFacts {
            failed_before_call: true,
            ..entry(Stage::Finished)
        };
        assert_eq!(
            one(&entry(Stage::Attempted), &now),
            failed(NO_PAYMENT_STARTED)
        );
        // That write not through yet: the entry is open, and pending.
        assert_eq!(
            one(&entry(Stage::Attempted), &entry(Stage::Attempted)),
            Some(PayOutcome::Pending { attempt_id })
        );
        // The method's failure, whatever the recheck has done with its own flag since.
        let now = EntryFacts {
            method_failed: true,
            ..entry(Stage::Finished)
        };
        assert_eq!(one(&entry(Stage::Paying), &now), failed(METHOD_FAILED));
        // The user's `done` outlasts the method's failure.
        let now = EntryFacts {
            user_resolved: Some(UserResolution::Done),
            method_failed: true,
            ..entry(Stage::Finished)
        };
        assert_eq!(one(&entry(Stage::RailFailed), &now), resolved);
        // A proof outlasts everything else.
        let now = EntryFacts {
            proof: true,
            user_resolved: Some(UserResolution::Failed),
            method_failed: true,
            ..entry(Stage::Finished)
        };
        assert!(matches!(
            one(&entry(Stage::Paying), &now),
            Some(PayOutcome::Paid { .. })
        ));
        // Abandoned before its attempt reached the claim: no attempt to report.
        assert_eq!(one(&entry(Stage::Recorded), &entry(Stage::Abandoned)), None);
    }

    #[test]
    fn test_resume_outcomes_report_every_entry_ended_until_it_is_told() {
        let first = AttemptId::from_bytes([1; 16]);
        let second = AttemptId::from_bytes([2; 16]);
        let entry = |attempt_id, stage, proof: bool, told: bool| EntryFacts {
            attempt_id,
            stage,
            proof,
            proof_msg_id: proof.then(|| "m".into()),
            method_failed: false,
            ask_again: false,
            failed_before_call: false,
            user_resolved: None,
            told,
            told_paid: false,
        };
        // Both paid and unwritten; the second's outcome applied by a batch sent at the
        // first's turn, so its own turn found it finished: both are reported.
        let before = [
            entry(first, Stage::RailPaid, true, false),
            entry(second, Stage::RailPaid, true, false),
        ];
        let after = [
            entry(first, Stage::Finished, true, false),
            entry(second, Stage::Finished, true, false),
        ];
        let out = resume_outcomes(&after, &Turns::default());
        assert!(
            matches!(
                out.as_slice(),
                [PayOutcome::Paid { attempt_id: a, .. }, PayOutcome::Paid { attempt_id: b, .. }]
                    if *a == first && *b == second
            ),
            "{out:?}"
        );
        assert_eq!(resume_disagreement(&before, &after, &out), None);
        // Ended by a call that returned an error, or never returned, so never told: the next
        // call reports it, though the entry was over before that call.
        let untold = [entry(first, Stage::Finished, true, false)];
        assert!(matches!(
            resume_outcomes(&untold, &Turns::default()).as_slice(),
            [PayOutcome::Paid { .. }]
        ));
        // Told: nothing, unless its payment was found made after all.
        let told = [entry(first, Stage::Finished, false, true)];
        assert!(resume_outcomes(&told, &Turns::default()).is_empty());
        let found = [entry(first, Stage::Finished, true, true)];
        assert!(matches!(
            resume_outcomes(&found, &Turns::default()).as_slice(),
            [PayOutcome::Paid { .. }]
        ));
        let unwritten = [entry(first, Stage::RailPaid, true, false)];
        assert!(matches!(
            resume_outcomes(&unwritten, &Turns::default()).as_slice(),
            [PayOutcome::PaidOutcomeUnwritten { .. }]
        ));
        // Told as failed, then opened again by the recheck with the proof: its open form,
        // and once finished, paid, however many calls lie between; once told paid, nothing.
        let reopened = [entry(first, Stage::RailPaid, true, true)];
        assert!(matches!(
            resume_outcomes(&reopened, &Turns::default()).as_slice(),
            [PayOutcome::PaidOutcomeUnwritten { .. }]
        ));
        assert!(matches!(
            resume_outcomes(&found, &Turns::default()).as_slice(),
            [PayOutcome::Paid { .. }]
        ));
        let told_paid = [EntryFacts {
            told_paid: true,
            ..entry(first, Stage::Finished, true, true)
        }];
        assert!(resume_outcomes(&told_paid, &Turns::default()).is_empty());
    }

    #[test]
    fn test_resume_disagreement_refuses_each_broken_rule() {
        let first = AttemptId::from_bytes([1; 16]);
        let second = AttemptId::from_bytes([2; 16]);
        let entry = |attempt_id, stage, proof: bool| EntryFacts {
            attempt_id,
            stage,
            proof,
            proof_msg_id: None,
            method_failed: false,
            ask_again: false,
            failed_before_call: false,
            user_resolved: None,
            told: false,
            told_paid: false,
        };
        let before = [
            entry(first, Stage::RailPaid, true),
            entry(second, Stage::RailPaid, true),
        ];
        let after = [
            entry(first, Stage::Finished, true),
            entry(second, Stage::Finished, true),
        ];
        let out = resume_outcomes(&after, &Turns::default());
        assert_eq!(resume_disagreement(&before, &after, &out), None);
        // An entry ended in the call and left out.
        assert!(resume_disagreement(&before, &after, &out[..1]).is_some());
        // One told twice.
        let twice = [out[0].clone(), out[0].clone(), out[1].clone()];
        assert!(resume_disagreement(&before, &after, &twice).is_some());
        // An entry gone from the journal before the call ends.
        assert!(resume_disagreement(&before, &after[..1], &out[..1]).is_some());
        // A proof lost.
        let lost = [entry(first, Stage::Finished, false), after[1].clone()];
        let told = resume_outcomes(&lost, &Turns::default());
        assert!(resume_disagreement(&before, &lost, &told).is_some());
        // A failure with no evidence that no payment was made.
        let ended = [entry(first, Stage::Finished, false)];
        let unfounded = [PayOutcome::Failed {
            attempt_id: first,
            reason: METHOD_FAILED.into(),
        }];
        assert!(
            resume_disagreement(&[entry(first, Stage::Paying, false)], &ended, &unfounded)
                .is_some()
        );
        // An outcome of no entry.
        let stranger = [PayOutcome::Pending {
            attempt_id: AttemptId::from_bytes([9; 16]),
        }];
        assert!(resume_disagreement(&[], &[], &stranger).is_some());
    }

    #[test]
    fn test_journal_entries_are_read_only_through_the_methods_word() {
        // Every read of a journal entry from the store lays what the method said over it
        // ([`JournalEntry::with_facts`]): the whole journal, one entry, a receipt's entry and a
        // restored one. A new read that skips it fails here.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        let mut reads = Vec::new();
        while let Some(dir) = stack.pop() {
            for item in std::fs::read_dir(&dir).unwrap() {
                let path = item.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let code = text.split("#[cfg(test)]\nmod tests {").next().unwrap();
                let lines: Vec<&str> = code.lines().collect();
                for (at, line) in lines.iter().enumerate() {
                    let typed = line.contains("::<JournalEntry>");
                    let keyed = line.contains("key::journal(") || line.contains("key::JOURNAL)");
                    let writes = ["put(", "delete(", "local(&key::journal"]
                        .iter()
                        .any(|write| line.contains(write));
                    if !typed && (!keyed || writes) {
                        continue;
                    }
                    let near = lines[at..(at + 8).min(lines.len())].join("\n");
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    assert!(near.contains(".with_facts("), "{name}:{}: {line}", at + 1);
                    reads.push(name);
                }
            }
        }
        reads.sort();
        assert_eq!(reads, ["pay.rs", "pay.rs", "protocol.rs", "receive.rs"]);
    }

    #[test]
    fn test_attempt_facts_only_grow_and_survive_every_older_copy() {
        use crate::journal::AttemptFacts;
        let proof: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"ref":"a"}"#).unwrap();
        let paid = AttemptFacts {
            proof: Some(proof.clone()),
            paid_at_ms: Some(5),
            method_failed: true,
            ask_again: false,
            told: false,
            told_paid: false,
        };
        // A write that would take the proof, the method's failure or told back is refused.
        for lost in [
            AttemptFacts {
                proof: None,
                paid_at_ms: None,
                ..paid.clone()
            },
            AttemptFacts {
                method_failed: false,
                ..paid.clone()
            },
        ] {
            assert!(!paid.grows_to(&lost), "{lost:?}");
        }
        let told = AttemptFacts {
            told: true,
            told_paid: false,
            ..AttemptFacts::default()
        };
        assert!(!told.grows_to(&AttemptFacts::default()));
        // The recheck's own flag may go.
        let asking = AttemptFacts {
            method_failed: true,
            ask_again: true,
            ..AttemptFacts::default()
        };
        assert!(asking.grows_to(&AttemptFacts {
            ask_again: false,
            ..asking.clone()
        }));
        // A restore keeps what either copy holds.
        let merged = AttemptFacts::default().merged(paid.clone());
        assert_eq!(merged.proof.as_ref(), Some(&proof));
        assert!(merged.method_failed && !merged.ask_again);
        assert!(paid.grows_to(&merged.clone().merged(asking.clone())));
        // Facts no write could leave are refused.
        assert!(AttemptFacts {
            paid_at_ms: Some(1),
            ..AttemptFacts::default()
        }
        .check()
        .is_err());
        assert!(AttemptFacts {
            ask_again: true,
            ..AttemptFacts::default()
        }
        .check()
        .is_err());
        assert!(paid.check().is_ok() && asking.check().is_ok());
    }

    fn is_client(error: &PaykitError, expected: &ClientRefusal) -> bool {
        matches!(error, PaykitError::Client(AppError::Refused(refusal)) if refusal == expected)
    }

    #[test]
    fn test_pay_gate_client_refusal_is_returned_from_a_first_handoff() {
        for refusal in [
            ClientRefusal::Stopped,
            ClientRefusal::Left,
            ClientRefusal::RestoreQuestionOpen,
            ClientRefusal::RecoveryRequired { generation: 3 },
        ] {
            let FirstHold::Return(error) = first_handoff_hold(Held::Client(refusal.clone())) else {
                panic!("{refusal:?} wrote the attempt failed");
            };
            assert!(is_client(&error, &refusal), "{error:?}");
        }
        // Writes go on: the attempt is closed, as before the client's refusal was typed.
        let wait = ClientRefusal::NotNow("reconcile mode".into());
        let FirstHold::Fail(Refusal::HeldForUser(crate::HeldForUserReason::SwitchClient(why))) =
            first_handoff_hold(Held::Client(wait.clone()))
        else {
            panic!("reconcile mode left the attempt open");
        };
        assert_eq!(why, wait.to_string());
    }

    #[test]
    fn test_pay_gate_repeat_is_pending_only_for_a_wait() {
        let attempt = AttemptId::from_bytes([4; 16]);
        for refusal in [
            ClientRefusal::Stopped,
            ClientRefusal::Left,
            ClientRefusal::RestoreQuestionOpen,
            ClientRefusal::RecoveryRequired { generation: 3 },
        ] {
            let held = repeat_hold(Held::Client(refusal.clone()), attempt);
            assert!(
                matches!(&held, Err(error) if is_client(error, &refusal)),
                "{held:?}"
            );
        }
        assert!(matches!(
            repeat_hold(
                Held::Client(ClientRefusal::NotNow("clocks".into())),
                attempt
            ),
            Ok(PayOutcome::Pending { .. })
        ));
        assert!(matches!(
            repeat_hold(
                Held::Check(Refusal::NotNow(crate::NotNowReason::ReadAgain("not yet"))),
                attempt
            ),
            Ok(PayOutcome::Pending { .. })
        ));
        // The payer pass a reconcile owes is waited for, never "may be paid" (Paykit spec 7.3).
        assert!(matches!(
            repeat_hold(Held::Check(crate::hold::payer_pass_refusal()), attempt),
            Ok(PayOutcome::Pending { .. })
        ));

        assert!(matches!(
            repeat_hold(Held::Check(Refusal::Destination("expired".into())), attempt),
            Ok(PayOutcome::Uncertain { .. })
        ));
    }

    #[test]
    fn test_pay_automatic_hold_is_the_users_to_answer() {
        use crate::floor::SharedFloor;
        for hold in [
            AutomaticHold::Blocked,
            AutomaticHold::Floor(SharedFloor::Known(Some(70))),
            AutomaticHold::Restore,
            AutomaticHold::Paused,
        ] {
            assert_eq!(
                hold.refusal(),
                Refusal::HeldForUser(crate::HeldForUserReason::HeldFromAutomatic),
                "{hold:?}"
            );
        }
    }

    #[test]
    fn test_pay_automatic_hold_names_a_divergent_floor() {
        assert_eq!(
            AutomaticHold::Floor(crate::floor::SharedFloor::Divergent).refusal(),
            Refusal::FloorHeld
        );
    }

    #[test]
    fn test_pay_automatic_hold_waits_for_a_floor_not_known() {
        use crate::floor::{FloorUnknown, SharedFloor};
        assert!(matches!(
            AutomaticHold::Floor(SharedFloor::Unknown(FloorUnknown::Unread)).refusal(),
            Refusal::NotNow(crate::NotNowReason::ReadAgain(why)) if why.contains("block floor")
        ));
        // A floor that doesn't read, or is gone, is no wait a read clears: paid by hand.
        for durable in [FloorUnknown::Unreadable, FloorUnknown::Gone] {
            assert_eq!(
                AutomaticHold::Floor(SharedFloor::Unknown(durable)).refusal(),
                Refusal::HeldForUser(crate::HeldForUserReason::HeldFromAutomatic)
            );
        }
    }

    #[test]
    fn test_pay_gate_resume_stops_only_at_a_wallet_wide_stop() {
        // A wallet-wide stop ends the resume, kept or not.
        let kept = |refusal| {
            PaykitError::Client(AppError::Kept {
                batch_id: [1; 16],
                error: Box::new(AppError::Refused(refusal)),
            })
        };
        for stop in [
            ClientRefusal::Stopped,
            ClientRefusal::Left,
            ClientRefusal::Gone,
            ClientRefusal::RestoreQuestionOpen,
            ClientRefusal::RecoveryRequired { generation: 3 },
        ] {
            assert!(stops_resume(&held_back(stop.clone())), "{stop:?}");
            assert!(stops_resume(&kept(stop)));
        }
        let alarm = AppError::Alarm(pubky_dispatch_client::app::Alarm::Rollback {
            reason: "seen".into(),
        });
        assert!(stops_resume(&PaykitError::Client(alarm)));
        assert!(stops_resume(&PaykitError::Client(AppError::Store(
            StoreError::Unavailable
        ))));
        let answer = |code: pubky_dispatch_core::http::ErrorCode| {
            PaykitError::Client(AppError::Switch(ClientError::Switch {
                status: code.status(),
                code,
                message: String::new(),
                current: serde_json::Value::Null,
                known_version: None,
                retry_after: None,
            }))
        };
        use pubky_dispatch_core::http::ErrorCode as E;
        // So does the Dispatch's answer that the app is no longer current, or that the account
        // awaits a recovery.
        assert!(stops_resume(&answer(E::NotCurrent)));
        assert!(stops_resume(&answer(E::CertExpired)));
        assert!(stops_resume(&answer(E::RecoveryRequired)));
        // A wait leaves its entry for the next resume and says nothing more: the Dispatch
        // client's refusal for now, kept or not, a service answer a retry may pass, the
        // network, the own Dispatch or identity.json for now, a conflict, the method's history
        // unreachable for now.
        // A client failure inside a kept batch, for each row: wrappers are judged by the
        // failure inside them (`AppError::Ahead`, which can't be built here, unwraps the same).
        let kept_of = |error: &PaykitError| match error {
            PaykitError::Client(inner) => Some(PaykitError::Client(AppError::Kept {
                batch_id: [2; 16],
                error: Box::new(inner.clone()),
            })),
            _ => None,
        };
        let unsigned = |status| {
            PaykitError::Client(AppError::Switch(ClientError::BadServiceSignature {
                status,
            }))
        };
        let transport = |error| PaykitError::Client(AppError::Transport(error));
        for waits in [
            answer(E::Conflict),
            answer(E::Unavailable),
            answer(E::RateLimited),
            answer(E::StaleTimestamp),
            PaykitError::Client(AppError::Switch(ClientError::BodyUnread {
                status: 429,
                code: E::RateLimited,
                retry_after: None,
            })),
            lost().into(),
            PaykitError::Rail(crate::RailError::Unavailable("busy".into())),
            PaykitError::Refused(Refusal::DispatchUnavailable("down".into())),
            PaykitError::Refused(Refusal::IdentityUnavailable("refresh".into())),
            PaykitError::Conflict("record".into()),
            // The pinned client's own temporary failures, kept or not (Dispatch spec 12.7,
            // Dispatch wire 7.1).
            PaykitError::Client(AppError::NotAnsweringAsAccount { since_ms: 1 }),
            PaykitError::Client(AppError::Kept {
                batch_id: [1; 16],
                error: Box::new(AppError::NotAnsweringAsAccount { since_ms: 1 }),
            }),
            unsigned(503),
            PaykitError::Client(AppError::Kept {
                batch_id: [1; 16],
                error: Box::new(AppError::Switch(ClientError::BadServiceSignature {
                    status: 502,
                })),
            }),
            PaykitError::Client(AppError::ExpiredUnsent),
            PaykitError::Client(AppError::Switch(ClientError::StaleAnswer {
                server_time_ms: 1,
            })),
            answer(E::Staged),
            transport(TransportError::Timeout),
            transport(TransportError::UnreadableSignedHeader { status: 504 }),
        ] {
            assert!(!stops_resume(&waits), "{waits:?}");
            assert!(entry_waits(&waits), "{waits:?}");
            // The same failure inside a kept batch is judged the same.
            if let Some(kept) = kept_of(&waits) {
                assert!(!stops_resume(&kept), "{kept:?}");
                assert!(entry_waits(&kept), "{kept:?}");
            }
        }
        // Anything else is no wait: a later resume meets it again until someone acts, so the
        // call returns it once every entry is visited.
        for returned in [
            kept(ClientRefusal::Usage("bound".into())),
            held_back(ClientRefusal::DenyListFull { over: 1 }),
            answer(E::PermissionMissing),
            answer(E::NotFound),
            answer(E::QuotaExceeded),
            unsigned(404),
            answer(E::Moved),
            PaykitError::Client(AppError::Switch(ClientError::InvalidResponse(
                "member".into(),
            ))),
            transport(TransportError::InvalidRequest("header".into())),
            held_back(ClientRefusal::NotNow("reconcile mode".into())),
            kept(ClientRefusal::NotNow("reconcile mode".into())),
            PaykitError::Client(AppError::Kept {
                batch_id: [1; 16],
                error: Box::new(AppError::Switch(ClientError::Switch {
                    status: 429,
                    code: E::QuotaExceeded,
                    message: String::new(),
                    current: serde_json::Value::Null,
                    known_version: None,
                    retry_after: None,
                })),
            }),
            transport(TransportError::FirstSendUnavailable),
            transport(TransportError::FirstSendExpired),
            PaykitError::Client(AppError::Switch(ClientError::BodyUnread {
                status: 413,
                code: E::TooLarge,
                retry_after: None,
            })),
            transport(TransportError::Refused("origin".into())),
            transport(TransportError::TooLarge(1)),
            transport(TransportError::UnreadableSignedHeader { status: 400 }),
            PaykitError::Client(AppError::Switch(ClientError::InvalidResponse(
                "member".into(),
            ))),
            PaykitError::Client(AppError::Switch(ClientError::Unpinned)),
            PaykitError::Client(AppError::InvalidData("member".into())),
            answer(E::StaleEpoch),
            answer(E::Moved),
            PaykitError::Refused(Refusal::HeldForUser(crate::HeldForUserReason::Unresolved(
                "reconcile",
            ))),
            PaykitError::Client(AppError::Transport(TransportError::Refused(
                "origin".into(),
            ))),
            PaykitError::Rail(crate::RailError::Refused("no".into())),
            PaykitError::Refused(Refusal::NotFound),
            PaykitError::Refused(Refusal::IdentityUnconfirmed),
        ] {
            assert!(!stops_resume(&returned), "{returned:?}");
            assert!(!entry_waits(&returned), "{returned:?}");
            if let Some(kept) = kept_of(&returned) {
                assert!(!stops_resume(&kept), "{kept:?}");
                assert!(!entry_waits(&kept), "{kept:?}");
            }
        }
        for stop in [answer(E::NotCurrent), held_back(ClientRefusal::Left)] {
            assert!(!entry_waits(&stop), "{stop:?}");
        }
        // This SDK's own wait is one: a later read clears it with no one acting.
        let wait = PaykitError::Refused(Refusal::NotNow(crate::NotNowReason::ReadAgain("read")));
        assert!(entry_waits(&wait) && !stops_resume(&wait));
    }

    #[test]
    fn test_pay_gate_payment_refuses_early_only_what_no_read_clears() {
        assert!(refused_for_good(&ClientRefusal::Stopped));
        assert!(refused_for_good(&ClientRefusal::Left));
        assert!(refused_for_good(&ClientRefusal::RestoreQuestionOpen));
        assert!(!refused_for_good(&ClientRefusal::RecoveryRequired {
            generation: 3
        }));
        assert!(!refused_for_good(&ClientRefusal::NotNow("clocks".into())));
    }

    fn lost() -> AppError {
        AppError::Transport(TransportError::Connect("down".into()))
    }

    const COPIES: [SwitchCopy; 4] = [
        SwitchCopy::Read,
        SwitchCopy::Unavailable,
        SwitchCopy::Stale,
        SwitchCopy::Pending,
    ];

    fn unread_copies() -> impl Iterator<Item = SwitchCopy> {
        COPIES.into_iter().filter(|c| *c != SwitchCopy::Read)
    }

    fn answer(copy: SwitchCopy, learned_nothing: bool, confirmed: bool) -> Answer {
        Answer {
            copy,
            learned_nothing,
            confirmed,
        }
    }

    #[test]
    fn test_fence_news_unread_names_only_the_states_known_to_read_nothing() {
        assert!(!unread(SwitchCopy::Read));
        for copy in [
            SwitchCopy::Unavailable,
            SwitchCopy::Stale,
            SwitchCopy::Pending,
        ] {
            assert!(unread(copy), "{copy:?}");
        }
    }

    #[test]
    fn test_fence_news_confirming_answer_goes_on_for_either_initiator() {
        for by in [Initiator::User, Initiator::Automatic] {
            for basis in COPIES {
                let confirming = answer(SwitchCopy::Read, false, true);
                assert_eq!(fence_news(true, basis, confirming, by), FenceNews::Confirms);
            }
        }
    }

    #[test]
    fn test_fence_news_same_document_unread_on_both_sides_is_none_by_hand() {
        for basis in unread_copies() {
            for copy in unread_copies() {
                for confirmed in [false, true] {
                    let unread = answer(copy, true, confirmed);
                    assert_eq!(
                        fence_news(true, basis, unread, Initiator::User),
                        FenceNews::None,
                        "{basis:?} {copy:?}"
                    );
                    // An automatic payment is no looser than before.
                    assert_eq!(
                        fence_news(true, basis, unread, Initiator::Automatic),
                        FenceNews::Holds,
                        "{basis:?} {copy:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_fence_news_answer_that_learned_anything_behind_an_unread_status_holds() {
        // The lookup learned something for this answer, though the identity it finished
        // with carries a failure: a copy read behind a newer lookup's failure, or a copy or
        // a freeze a service gave before the lookup failed. That is news.
        for by in [Initiator::User, Initiator::Automatic] {
            for basis in unread_copies() {
                for copy in unread_copies() {
                    for confirmed in [false, true] {
                        assert_eq!(
                            fence_news(true, basis, answer(copy, false, confirmed), by),
                            FenceNews::Holds,
                            "{basis:?} {copy:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_fence_news_read_basis_with_an_unread_answer_holds() {
        for by in [Initiator::User, Initiator::Automatic] {
            for copy in unread_copies() {
                for (learned_nothing, confirmed) in [(true, false), (true, true), (false, false)] {
                    assert_eq!(
                        fence_news(
                            true,
                            SwitchCopy::Read,
                            answer(copy, learned_nothing, confirmed),
                            by
                        ),
                        FenceNews::Holds,
                        "{copy:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_fence_news_another_document_or_an_unconfirmed_read_holds() {
        for by in [Initiator::User, Initiator::Automatic] {
            for basis in COPIES {
                for copy in COPIES {
                    for learned_nothing in [false, true] {
                        for confirmed in [false, true] {
                            assert_eq!(
                                fence_news(
                                    false,
                                    basis,
                                    answer(copy, learned_nothing, confirmed),
                                    by
                                ),
                                FenceNews::Holds,
                                "{basis:?} {copy:?}"
                            );
                        }
                    }
                }
                // A read Dispatch copy of the same document that doesn't confirm it is news.
                assert_eq!(
                    fence_news(true, basis, answer(SwitchCopy::Read, false, false), by),
                    FenceNews::Holds,
                    "{basis:?}"
                );
            }
        }
    }

    #[test]
    fn test_restored_payment_compares_the_signed_occurrence_and_method_result() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../specs/v0.2/vectors/sealed.json")).unwrap();
        let period = vectors["activity_records"]["accepted"].as_array().unwrap().iter()
            .find(|r| r["name"] == "a period record whose proof names earlier details, with later ones in earlier_details").unwrap();
        let value: serde_json::Value =
            serde_json::from_str(period["value"].as_str().unwrap()).unwrap();
        let payee =
            pubky_dispatch_core::enc::pubky_key(value["counterparty"].as_str().unwrap()).unwrap();
        let signed = StoredMessage::read(&value["proof"], &payee).unwrap();
        let Body::PaymentProof(proof) = signed.body() else {
            panic!("expected a payment proof")
        };
        let payer = signed.sender();
        let app = signed.cert().app_id();
        let refs = &signed.message().refs;
        let matches = |expected: &PaymentProof| {
            matches_paid_proof(&signed, payer, app, payee, refs, expected)
        };
        assert!(matches(proof));
        assert!(!matches_paid_proof(&signed, payee, app, payer, refs, proof));
        assert!(!matches_paid_proof(
            &signed, payer, [0; 16], payee, refs, proof
        ));
        assert!(!matches_paid_proof(
            &signed,
            payer,
            app,
            payee,
            &[[0; 32]],
            proof
        ));
        let mut other = proof.clone();
        other.attempt_id = AttemptId::from_bytes([0; 16]);
        assert!(!matches(&other));
        let mut other = proof.clone();
        other.billing_period.as_mut().unwrap().ends_at += 1;
        assert!(!matches(&other));
        let mut other = proof.clone();
        other.payment_app_id = [0; 16];
        assert!(!matches(&other));
        let mut other = proof.clone();
        other.proof.clear();
        assert!(!matches(&other));
    }

    #[test]
    fn test_pricing_period_without_rates_keeps_the_requested_asset_constraint() {
        let requested = PaymentAmount {
            value: paykit_lib::Decimal::new("1.001").unwrap(),
            asset: paykit_lib::Asset::new("usd").unwrap(),
        };
        let request = RequestRef {
            payee: [1; 32],
            request_hash: [2; 32],
        };
        let terms = paykit_lib::bodies::Conversion::PerPeriod;
        let pricing = Pricing::of_request(&requested, request, None, Some(&terms), Initiator::User);
        let rail = crate::rail::FakeRail::new();
        let own = PaymentEndpointIdentifier::new("usd-ach-account").unwrap();
        let other = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let Priced::Plain(amount) = pricing.price(&rail, &own, |_| true).0 else {
            panic!("unpriced requested asset")
        };
        assert_eq!(amount.value, paykit_lib::Decimal::new("1.01").unwrap());
        assert!(matches!(
            pricing.price(&rail, &other, |_| true).0,
            Priced::Unusable(_)
        ));
        let plain = Pricing::of_request(&requested, request, None, None, Initiator::User);
        assert!(matches!(
            plain.price(&rail, &other, |_| true).0,
            Priced::ByHand(_)
        ));
    }

    #[test]
    fn test_unpriced_now_refuses_by_hand_on_an_automatic_first_handoff() {
        let rail = crate::rail::FakeRail::new();
        let usd = PaymentAmount {
            value: paykit_lib::Decimal::new("10").unwrap(),
            asset: paykit_lib::Asset::new("usd").unwrap(),
        };
        let price = |endpoint: &str| {
            let endpoint = PaymentEndpointIdentifier::new(endpoint).unwrap();
            conversion::price(&rail, &usd, None, &endpoint)
        };
        // A payment made only by hand: refused on an automatic first handoff, and held to
        // its user's confirmation otherwise.
        let refused = unpriced_now(price("eur-sepa-iban"), true).unwrap();
        assert!(
            matches!(&refused, Err(Refusal::UnsupportedEndpoint(why)) if why.contains("only by hand")),
            "{refused:?}"
        );
        assert_eq!(
            unpriced_now(price("eur-sepa-iban"), false).unwrap(),
            Ok((usd.clone(), true))
        );
        // An endpoint of the requested asset is paid automatically, as before.
        assert_eq!(
            unpriced_now(price("usd-ach-account"), true).unwrap(),
            Ok((usd.clone(), false))
        );
        // A selection without rates never gives an amount a rate prices.
        let rate = paykit_lib::Rate::new(
            paykit_lib::Asset::new("btc").unwrap(),
            paykit_lib::Decimal::new("1").unwrap(),
        )
        .unwrap();
        let priced = Priced::ByRate {
            rate,
            amount: usd.clone(),
        };
        assert!(unpriced_now(priced, false).is_err());
    }

    #[test]
    fn test_handoff_period_rates_require_the_signed_source() {
        assert!(rates_usable(&RateSource::Request, None, 0, None));
        let details = RateSource::PeriodDetails {
            msg_id: "details".into(),
        };
        assert!(!rates_usable(&details, None, 0, None));
    }

    #[test]
    fn test_prune_bound_passed_before_the_service_time() {
        assert!(super::bound_passed(10, Some(11), 11));
        assert!(!super::bound_passed(10, Some(10), 11));
    }

    #[test]
    fn test_prune_bound_not_passed_by_a_far_service_time_or_without_one() {
        let day = pubky_dispatch_core::records::DAY_MS;
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
    fn test_dispatch_unavailable_through_a_kept_batch_and_the_erasure_wait() {
        assert!(dispatch_unavailable(&lost()));
        assert!(dispatch_unavailable(&AppError::Kept {
            batch_id: [1; 16],
            error: Box::new(lost()),
        }));
        assert!(dispatch_unavailable(&AppError::NotAnsweringAsAccount {
            since_ms: 1
        }));
        assert!(dispatch_unavailable(&AppError::Switch(
            ClientError::BadServiceSignature { status: 503 }
        )));
    }

    #[test]
    fn test_dispatch_unavailable_not_for_an_answer_it_signed() {
        assert!(!dispatch_unavailable(&AppError::Switch(
            ClientError::BadServiceSignature { status: 404 }
        )));
        assert!(!dispatch_unavailable(&AppError::Kept {
            batch_id: [1; 16],
            error: Box::new(AppError::Switch(ClientError::BadServiceSignature {
                status: 404
            })),
        }));
    }

    fn discovery_ask() -> serde_json::Value {
        serde_json::to_value(ListAsk {
            payee: [3; 32],
            app_id: None,
            held_version: None,
            asked_at_ms: 7,
        })
        .unwrap()
    }

    #[test]
    fn test_discovery_state_ask_unknown_member() {
        let mut value = discovery_ask();
        value["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ListAsk>(value).is_err());
    }

    #[test]
    fn test_discovery_state_ask_requires_app_member() {
        let mut value = discovery_ask();
        value.as_object_mut().unwrap().remove("app_id").unwrap();
        assert!(serde_json::from_value::<ListAsk>(value).is_err());
    }

    #[test]
    fn test_discovery_state_ask_requires_version_member() {
        let mut value = discovery_ask();
        value
            .as_object_mut()
            .unwrap()
            .remove("held_version")
            .unwrap();
        assert!(serde_json::from_value::<ListAsk>(value).is_err());
    }

    #[test]
    fn test_discovery_state_ask_current_structure() {
        for (app_id, held_version) in [
            (None, None),
            (Some([4; 16]), None),
            (
                Some([4; 16]),
                Some(paykit_lib::ListVersion::new(1, 9).unwrap()),
            ),
        ] {
            let value = ListAsk {
                payee: [3; 32],
                app_id,
                held_version,
                asked_at_ms: 7,
            };
            let bytes = serde_json::to_vec(&value).unwrap();
            assert_eq!(serde_json::from_slice::<ListAsk>(&bytes).unwrap(), value);
            let raw = String::from_utf8(bytes).unwrap();
            for (member, duplicate) in [
                ("app_id", "null"),
                ("held_version", "null"),
                ("asked_at_ms", "7"),
            ] {
                let twice = raw.replacen("{", &format!("{{\"{member}\":{duplicate},"), 1);
                assert!(serde_json::from_str::<ListAsk>(&twice).is_err());
            }
        }
        for (member, bad) in [
            ("app_id", serde_json::json!("not-an-array")),
            ("held_version", serde_json::json!(-1)),
            ("asked_at_ms", serde_json::Value::Null),
        ] {
            let mut value = discovery_ask();
            value[member] = bad;
            assert!(serde_json::from_value::<ListAsk>(value).is_err());
        }
        for member in ["payee", "asked_at_ms"] {
            let mut value = discovery_ask();
            value.as_object_mut().unwrap().remove(member).unwrap();
            assert!(serde_json::from_value::<ListAsk>(value).is_err());
        }
    }
}
