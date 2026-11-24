//! The SDK's error type ([`PaykitError`]) and Paykit's local refusals ([`Refusal`]).
//!
//! No text quotes a body, a payload, a key or a document: each names the rule or the member
//! that failed.

use pubky_switch_client::app::AppError;
use pubky_switch_client::error::{StoreError, TransportError};
use pubky_switch_core::http::ErrorCode;

use crate::rail::RailError;

/// A rule of Paykit that refuses a call. Nothing was paid, and nothing the refusal names was
/// written; the text in a variant names the rule, never content.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Refusal {
    /// The app holds no such request, record or payment.
    #[error("no such request, record or payment")]
    NotFound,
    /// The call needs a scope the app's certificate doesn't hold, such as `paykit_pay` to pay or
    /// `paykit_request` to send a request (Paykit spec 3.1).
    #[error("Paykit spec 3.1: the certificate doesn't hold {0}")]
    ScopeMissing(String),
    /// A labelled selling app sends every request under one of its labels, and only under a
    /// label it holds (Paykit spec 3.1, 8.1).
    #[error("Paykit spec 3.1: {0}")]
    Label(String),
    /// The request was rejected by this identity (Paykit spec 8.1), or the Allowance by
    /// either side (Paykit spec 8.3): nothing more is done under it.
    #[error("the request or Allowance was rejected")]
    Rejected,
    /// Restored from a backup, or past a `missed` item, the app holds the request: the payee
    /// may have cancelled it since the backup, or among the entries the service deleted. It
    /// is paid only once the user has seen that
    /// ([`crate::PayOptions::possibly_cancelled_confirmed`]; Paykit spec 7.2, "What a
    /// restore loses").
    #[error(
        "Paykit spec 7.2: held after a restore or a missed item; the payee may have cancelled it"
    )]
    HeldAfterRestore {
        /// The request and current hold to show before asking the user to confirm.
        held: Box<crate::HeldRequest>,
    },
    /// The request was cancelled by either side (Paykit spec 8.1).
    #[error("the request was cancelled")]
    Cancelled,
    /// The Allowance has ended, by either side, by its proposal's withdrawal or when its
    /// terms expire, or this app keeps the counterparty's end as its stop: nothing more is
    /// done under it, and no later call changes that (Paykit spec 8.3).
    #[error("Paykit spec 8.3: the Allowance has ended")]
    AllowanceEnded,
    /// The Allowance is conflicted: a second, different proposal or response came, so
    /// nothing is answered, ended, granted or paid under it, for good (Paykit spec 8.3,
    /// "Conflicted").
    #[error("Paykit spec 8.3: the Allowance is conflicted")]
    AllowanceConflicted,
    /// The user must confirm this Allowance again before this app pays under it: after a
    /// `missed` item, or a restore until it is judged clean or the user confirms, an
    /// executor pays under no Allowance on its consent list until the user has confirmed it
    /// again, since an end may be among what was lost (Paykit spec 8.3, "Ends reach the
    /// executor"). After a provider loss, an Allowance this app executes that the user last
    /// confirmed before the loss gets it too: confirming writes its grant at the current
    /// generation, but automatic payments under it don't resume in this version, which
    /// doesn't repair its usage: they answer [`Refusal::NotNow`] with that reason, and the
    /// user who wants them ends the Allowance and sets up a new one (Paykit spec 8.3, "After
    /// provider loss"). It is
    /// returned only where confirming lifts it, so not while the Allowance
    /// is ended, expired, executed by another app or its counterparty blocked. The host shows
    /// the Allowance `hold` names and, once the user confirms, calls
    /// [`crate::Paykit::confirm_allowance_executor`] with `hold`; if that answers
    /// [`Refusal::NotNow`], it keeps the confirmation with `hold` and calls again later,
    /// except for the repair after a provider loss, which no confirmation lifts. The
    /// confirmation counts only for the hold it was given under: after a newer restore,
    /// `missed` item or provider loss the call answers this refusal again, with the new hold,
    /// and the host asks the user again.
    #[error("Paykit spec 8.3: the user must confirm this Allowance again")]
    AllowanceConfirmationNeeded {
        /// The Allowance to confirm, with the hold in force when it was shown.
        hold: crate::AllowanceHold,
    },
    /// A payee activity record lost credit history to a delete. A new receipt cannot
    /// be issued for this occurrence, including by hand (Paykit spec 6).
    #[error("Paykit spec 6: the payee record lost credit history to a delete")]
    CreditHistoryLost,
    /// The obligation is paid: its claim is SETTLED, or its record holds the proof of a
    /// payment (Paykit spec 7.2).
    #[error("already paid")]
    AlreadyPaid,
    /// Another own app has an open attempt on the obligation: "being handled by your other
    /// wallet" (Paykit spec 7.1, 8.5).
    #[error("being handled by your other wallet")]
    BeingHandledByOtherWallet,
    /// The obligation may already be paid: an `unknown` or orphaned attempt, a next attempt
    /// that would be flagged `possibly_duplicate`, a pinned deadline other than the app's own,
    /// an attempt a restore left unresolved, or an obligation payable before a provider
    /// loss (Paykit spec 7.3). Only a user who has seen "may already be
    /// paid" pays it (Paykit spec 7.1, 7.2). `doubt`, when there is one, is what the user
    /// is shown and may pay over: paying anyway carries it back
    /// ([`crate::PayOptions::with_may_be_paid_confirmed`]), and only that same doubt is
    /// passed. `None` for a reason no confirmation passes, such as a pinned deadline other
    /// than the app's own, a claim that doesn't show this app's attempt open, or a
    /// Billing Period with incomplete payment history after payer-provider loss.
    #[error("may already be paid: {reason}")]
    MayBePaid {
        /// The rule that makes it doubtful.
        reason: String,
        /// What the user may pay over.
        doubt: Option<Box<crate::PaymentDoubt>>,
    },
    /// The user's answer names a provider loss that no longer decides the doubt it answers
    /// (Paykit spec 7.3): a newer loss is kept, or followed before its step has run, since
    /// the doubt was shown. `decisive` is the loss that decides now, `None` for none; the
    /// host shows the doubt again under it. Nothing was written.
    #[error("Paykit spec 7.3: the provider loss shown no longer decides the doubt")]
    LossChanged {
        /// The loss that decides the doubt now.
        decisive: Option<crate::LossBoundary>,
    },

    /// The attempt's deadline has passed, or the request's proposal expired (Paykit spec 7.1).
    #[error("Paykit spec 7.1: the deadline has passed")]
    DeadlinePassed,
    /// A recurring request is paid per Billing Period (Paykit spec 8.2): `pay_request`
    /// refuses it, and [`crate::Paykit::pay_period`] pays it.
    #[error("Paykit spec 8.2: a recurring request is paid per Billing Period")]
    Recurring,
    /// Whether this payment is credited already to another occurrence of the payer's can't
    /// be told now: a record of the payee's the app holds can't be opened, or its copy of
    /// them may lack records. One payment credits one occurrence, so nothing is credited
    /// or receipted. Tried again once that clears; or credited when the user, shown that
    /// it may be credited already, confirms
    /// ([`crate::ReceiptDetails::with_maybe_credited_confirmed`]).
    #[error(
        "Paykit spec 8.4: whether this payment is credited to another occurrence can't be told now"
    )]
    MaybeCredited,
    /// The payment method verified the payment, but its verified payment time, `paid_at_ms`,
    /// lies outside what the occurrence allows: before the request's time less the method's
    /// declared tolerance, after the occurrence's deadline, or outside the validity of the
    /// destination or the rates it was paid by (Paykit spec 7.1, 8.2). Nothing is
    /// credited or receipted on this evidence: the occurrence stays unpaid, and the funds
    /// are the payee's received funds, for its user to settle with the payer.
    #[error("Paykit spec 7.1: the verified payment time lies outside what the request allows")]
    PaymentTimeOutside {
        /// The verified payment time, in Unix milliseconds.
        paid_at_ms: u64,
    },
    /// The settlement mark cannot be stored. A receipt without it leaves other
    /// occurrences at risk of a second credit (Paykit wire 5.7, Full).
    #[error("Paykit wire 5.7: bucket {bucket} cannot store the mark; this payment may be credited again")]
    SettlementBucketFull {
        /// The settlement bucket that could not accept the mark.
        bucket: u16,
    },
    /// The recurring request is accepted already, by this app or another own app (Paykit
    /// spec 8.2): it is neither accepted nor rejected again, only cancelled.
    #[error("Paykit spec 8.2: the subscription is accepted already")]
    Subscribed,
    /// The payer hasn't accepted the recurring request: as the payee's record shows it, its
    /// period details wait for the acceptance, as v0.1's quotes did; as the payer's request
    /// record shows it, no Billing Period is paid before it (Paykit spec 8.2).
    #[error("Paykit spec 8.2: the payer hasn't accepted the subscription")]
    NotSubscribed,
    /// The Billing Period hasn't started on the latest service time verified: it is paid
    /// only by the user, after confirming paying it early
    /// ([`crate::PayOptions::with_early_payment_confirmed`]; Paykit spec 8.2, "Not before its
    /// start").
    #[error("Paykit spec 8.2: the Billing Period hasn't started")]
    PeriodNotStarted,
    /// Not paid by this call's automatic payer (Paykit spec 8.2, 8.3), in one of three cases:
    /// - the occurrence carries a manual-only mark: no wallet's policy and no Allowance's
    ///   executor pays it automatically, and it is left for the user;
    /// - asked of the wallet's own policy ([`crate::Paykit::pay_period_automatically`]), the
    ///   Billing Period is funded under an Allowance: it is left for that Allowance's executor
    ///   ([`crate::Paykit::pay_period_under_allowance`]);
    /// - asked of an executor ([`crate::Paykit::pay_period_under_allowance`]), the Billing
    ///   Period is funded under no Allowance: it is left for the wallet's own policy or the
    ///   user.
    #[error("Paykit spec 8.2, 8.3: this occurrence cannot be paid by this automatic payer")]
    ManualOnly,
    /// Not paid automatically, for good: the authenticated subscription and service time
    /// don't authorize this automatic Billing Period (Paykit spec 8.2); the request or
    /// Billing Period doesn't qualify under the Allowance it is paid under, outside its terms
    /// or with its lifetime limit used up and nothing reserved or to release; the Allowance
    /// the payment was reserved under expired before its first dispatch; or the Allowance was
    /// set up before a provider loss and never granted here, so it can't be granted now and
    /// the user can end it and set up a new one (Paykit spec 8.3, "After provider loss"); or,
    /// after a provider loss, the request is one-time, under the shared post-loss hold
    /// (Paykit spec 8.1). The user may still pay it by hand. At an open
    /// attempt's first dispatch this is an [`Self::AttemptFailed`].
    #[error("Paykit spec 8.2, 8.3: not paid automatically: {0}")]
    NotAutomatic(String),
    /// The request record lists 16 Billing Periods that may have an open attempt and none of
    /// them has finished: no attempt is made on another period until one does (Paykit spec
    /// 8.2, "Attempted periods").
    #[error("Paykit spec 8.2: 16 Billing Periods may have an open attempt")]
    AttemptedPeriodsFull,
    /// The destination check failed (Paykit spec 4, 7.2): the text names the rule. Refused
    /// before the attempt was written, nothing was written; refused after it, the refusal
    /// comes as [`Refusal::AttemptFailed`].
    #[error("Paykit spec 7.2: destination refused: {0}")]
    Destination(String),
    /// The attempt was written, and a check before money moved then refused the payment: the
    /// app wrote `failed` on it instead of paying, and nothing was paid (Paykit spec 7.2 step
    /// 1). The inner refusal names the check. Any other refusal of a payment was made before
    /// the attempt, and wrote nothing.
    #[error("Paykit spec 7.2: `failed` written: {0}")]
    AttemptFailed(Box<Refusal>),
    /// No usable destination: no endpoint of the request, no usable Private Payment List, no
    /// usable public endpoint, or none the caller named (Paykit spec 4, 8.1, 8.4).
    #[error("no usable destination: {0}")]
    NoDestination(String),
    /// The destination offers endpoints the payment may use, but the payment method pays
    /// none of them ([`crate::Rail::select_endpoints`]), or the caller named one it doesn't
    /// pay. Nothing was written. Another list or request from the payee, or another payment
    /// method, may serve.
    #[error("no endpoint the payment method pays: {0}")]
    UnsupportedEndpoint(String),
    /// No usable Private Payment List from the payee: none held, or each one expired, of a
    /// version used up by a single-use endpoint of it reserved, or of a generation above the
    /// payee's verified `identity.json` (Paykit spec 8.4, "No usable list"; Paykit wire
    /// 4.13); also when lists in use offer only endpoints the payment method, the conversion
    /// terms, the caller's endpoint or a request's accepted identifiers leave unusable,
    /// beside one used up, or, for the first two, beside one expired, whose newer list may
    /// serve ([`crate::Rail::select_endpoints`]). The app asked the payee for its current list with a
    /// `paykit.private_payment_list_request`; the payee can't be forced to answer, so the
    /// user is told the app is waiting for the payee's list. The user may choose to pay to
    /// the payee's public endpoints instead ([`crate::PaymentMode::Public`]); the app never
    /// switches on its own. A list in [`crate::Paykit::held_lists`] waits too, because its
    /// record was deleted, doesn't read, or is absent: no held list's record is created over
    /// an absent key, which can be a delete the service forgot (Paykit spec 6; wire 5). After
    /// a delete, or over an absent key, it can wait indefinitely, and the payee's public
    /// endpoints or a request's own destination remain the way to pay.
    #[error(
        "Paykit spec 8.4: waiting for the payee's Private Payment List{}",
        if *device_clock_behind { "; a list expired by the service's time: the device clock is behind it" } else { "" }
    )]
    WaitingForList {
        /// Whether a list of the payee's has expired only by the service's time: its expiry
        /// is at or before the highest service time verified, but after the device clock,
        /// which is behind the service's time. The user is told so (Paykit spec 7.2, check
        /// the destination).
        device_clock_behind: bool,
    },
    /// The client gave up this counterparty's current Private Payment List with an
    /// unknown outcome. If it was a withdrawal, the counterparty may keep paying the
    /// earlier list; if not, it may lack the list. Nothing more is sent on its own: only
    /// the user, shown this value, can authorize another version
    /// ([`crate::Paykit::send_list_again`]; Paykit spec 8.4, 9). Unlike other refusals, the
    /// call that returns it may have recorded the host's choice first:
    /// [`crate::Paykit::withdraw_list_from`] records the counterparty as withdrawn.
    #[error("Paykit spec 9: the delivery of the counterparty's current list is unknown")]
    ListDeliveryUnknown(Box<crate::UnknownListDelivery>),
    /// The payment method would transfer another amount or asset, an amount it can't tell, or
    /// a fee above the app's cap, and the user didn't choose it (Paykit spec 7.2 step 1).
    /// `quote` is what it would transfer, to show the user; paying it anyway carries it back
    /// ([`crate::PayOptions::with_transfer_confirmed`]).
    #[error("Paykit spec 7.2 step 1: {reason}")]
    Transfer {
        /// Which rule the transfer breaks.
        reason: String,
        /// The destination and what it would transfer.
        quote: Box<crate::TransferQuote>,
    },
    /// This quote needs authorization for this payment (Paykit spec 7.2 step 1, 8.2).
    /// At selection, no attempt has been opened. The host may confirm the exact quote with
    /// the user or apply the user's bounded policy for this payment and return the quote
    /// through [`crate::PayOptions::with_rate_authorized`] or [`crate::Paykit::pay_period_automatically`].
    /// An Allowance cannot authorize repricing. A quote whose rate expired or whose computed
    /// amount changed before dispatch fails the first attempt. A repeat that may already
    /// have been submitted stays [`crate::PayOutcome::Uncertain`] on its original terms.
    #[error(
        "Paykit spec 7.2 step 1: the rate and computed amount need authorization for this payment"
    )]
    PricedByRate {
        /// What the user is shown: the amount computed, the rate and the destination.
        payment: Box<crate::PricedPayment>,
    },
    /// The payee's `identity.json` couldn't be read, or the app's own couldn't be revalidated,
    /// before money moves (Paykit spec 4, 7.2).
    #[error("Paykit spec 7.2: {0}")]
    IdentityUnavailable(String),
    /// The payee's switch couldn't confirm its `identity.json` in time: a payment the user
    /// started goes on only once the user confirms paying although the identity's switch
    /// couldn't confirm its apps (Switch spec 4.3, "No silent fallback"; Paykit spec 7.2).
    /// Nothing was written.
    #[error(
        "Switch spec 4.3: the payee's switch couldn't confirm its apps; the user must confirm"
    )]
    IdentityUnconfirmed,
    /// The claim's attempt is open: a rejection or cancellation is refused while a payment is
    /// in progress (Paykit spec 8.1).
    #[error("Paykit spec 8.1: a payment is in progress")]
    PaymentInProgress,
    /// The own `identity.json` doesn't authorize the payment: it doesn't list this app with
    /// the certificate it holds, or names another service or generation than the one the
    /// attempt was written at, after a provider change (Paykit spec 4, Switch spec 12.5,
    /// 12.6). Nothing is paid; the attempt is resolved by the move rules: the claim at the
    /// service followed, or else "may be paid".
    #[error("not current here: {0}")]
    NotCurrent(String),
    /// The client layer holds the attempt back: an alarm, a restore question, reconcile mode,
    /// or the clocks disagreeing (Switch spec 10.1, 12.4).
    #[error("not now: {0}")]
    NotNow(String),
    /// The identity's block floor was seen divergent, and nothing can show what the hidden
    /// upload held: no raise of the floor can be written, so an unblock or a widening that
    /// needs one waits, and no automatic one-time payment goes (Paykit wire 5.6, Divergent
    /// floor). Payments by hand, Billing Periods and payments without a request go on under
    /// their own rules. Retrying changes nothing until the shared post-loss hold stands too,
    /// when the unblock or widening goes without the raise (Unblocking under the divergence
    /// hold).
    #[error("Paykit wire 5.6: the block floor is held after it was seen divergent, so no raise of it can be written and this waits until the shared post-loss hold stands too; each request can still be paid by hand, and Billing Periods and payments without a request go on")]
    FloorHeld,
    /// A value the caller gave breaks a rule: the text names it.
    #[error("usage: {0}")]
    Usage(String),
    /// `done` waits for the payer's activity record, which it never goes without (Paykit
    /// spec 7.2 step 3): the record holds a value that doesn't read, with no copy this app
    /// trusts, or is gone with nothing to rebuild it from. Nothing was written.
    #[error("the payer's activity record can't be updated now: {0}")]
    RecordHeld(String),
    /// The app's certificate fails Paykit's checks (Paykit wire 3.1), though the Switch took
    /// it: at a join, before anything is written, or after a renewal the client adopted. The
    /// app takes no Paykit action under it until a certificate that passes replaces it.
    /// Nothing was written.
    #[error("Paykit wire 3.1: the app's certificate fails Paykit's checks: {0}")]
    CertificateRefused(String),
    /// The app's own switch doesn't answer: the network, a `503`, a `5xx` its key didn't sign,
    /// or, while the account may have been erased, the wait before the app takes it as gone
    /// (`AppError::NotAnsweringAsAccount`, Switch spec 12.7). Paykit spec 8.4 lets a payment
    /// without a request to a reusable public endpoint go ahead without it; this SDK doesn't
    /// make one, and pays only attempt-first (Paykit spec 7.2). Nothing was written or paid.
    #[error("Paykit spec 8.4: the own switch doesn't answer: {0}")]
    SwitchUnavailable(String),
    /// The identity's deny list holds the payee: what is open with it is paused, and its
    /// request is paid only once the user confirms (Paykit spec 8.1, "Blocked
    /// counterparties"). `payment` is what the user is shown; paying anyway carries it back
    /// ([`crate::PayOptions::with_blocked_confirmed`]). Nothing was written.
    #[error("Paykit spec 8.1: the payee is blocked; the user must confirm")]
    Blocked {
        /// The payment the user is shown.
        payment: Box<crate::BlockedPayment>,
    },
    /// A record the call acts on is divergent after a provider loss, or was seen so, and the
    /// user hasn't confirmed it (Paykit spec 6, "Divergent records"): its upload may hold
    /// what the current write lacks, such as a cancellation of the request, a receipt, or a
    /// newer list in which the payee withdrew a destination. Nothing was written or paid.
    /// The host shows `records` and, once the user has seen that, calls
    /// [`crate::Paykit::confirm_divergent`] for each, as given here, then calls again. A
    /// record given here with [`crate::DivergentRecord::writes_waiting`] is confirmed
    /// already, and confirming it again writes nothing: only a receive writes the messages
    /// kept over it, so the host runs [`crate::Paykit::receive`] until
    /// [`crate::Paykit::divergent_records`] shows it without `writes_waiting`, or no longer
    /// lists it, and only then calls again. From
    /// [`crate::Paykit::confirm_payment`], each record is shown with
    /// [`crate::DivergentRecord::may_hold_credit`]: only a confirmation of these lets the
    /// credit go, so this refusal comes also after the user confirmed the same record from a
    /// receive's summary or [`crate::Paykit::divergent_records`], and is answered the same
    /// way. A payment's refusal never names the list of an app the payee's `identity.json`
    /// doesn't list in `apps`, or lists as removed, nor the record of a request past its
    /// deadline, carrying endpoints of an app that copy doesn't list as current, or
    /// requiring an app it lists as removed: such a payment goes on to the refusal of what
    /// stops it ([`crate::Paykit::pay_request`]).
    #[error("Paykit spec 6: a record is divergent; the user must confirm")]
    Divergent {
        /// The records the user is asked to confirm.
        records: Vec<crate::DivergentRecord>,
    },
}

/// A failure of the SDK.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum PaykitError {
    /// A failure of the Switch client, carried as it reports it: the switch's answers with
    /// their error codes and retry rules, the transport, the store, alarms and the client's
    /// own refusals.
    #[error(transparent)]
    Client(AppError),
    /// The requested public endpoint edit is durable but has no signed record yet.
    /// The reported signing refusal must be resolved before the client can publish it.
    #[error("public endpoint change pending: {0}")]
    PublicEndpointChangePending(#[source] AppError),
    /// A Paykit rule of `paykit-lib`: caller input that fails a structural check
    /// (`Validation`), or network or shared data that breaks a rule (`Invalid`).
    #[error(transparent)]
    Lib(paykit_lib::Error),
    /// The payment method failed. A failure after a payment may have been submitted never
    /// means the payment failed: the attempt stays open (Paykit spec 7.2).
    #[error(transparent)]
    Rail(RailError),
    /// A Homeserver couldn't be read now.
    #[error("homeserver: {0}")]
    Homeserver(TransportError),
    /// A rule of Paykit refused the call.
    #[error("refused: {0}")]
    Refused(Refusal),
    /// What the call was built on changed meanwhile, at the switch or in another own app: the
    /// caller reads again and decides again. The text names what changed.
    #[error("conflict: {0}")]
    Conflict(String),
}

impl PaykitError {
    /// The switch's error code, when the failure is an answer of the switch, also one that
    /// left the call's batch kept ([`AppError::Kept`]).
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Client(error) => error.code(),
            _ => None,
        }
    }

    /// The ID of the call's own batch, when the client kept it to send again
    /// ([`AppError::Kept`]): what it writes and sends goes, the same bytes, at the app's next
    /// write or read, so the caller doesn't make the call again. The batch keeps this ID for
    /// its whole life, signed again or not, and the client's `App::kept_batch` (through
    /// [`crate::Paykit::app`]) tells what became of it: still kept, applied, or dropped and
    /// why, for `SENT_RETENTION_MS` after it ended. `None` from there never says the batch
    /// was never sent. `None` here when nothing of the
    /// call was kept, but for a store failure: the client returns that bare when it can't
    /// read whether it kept the batch, which may still go with the next write or read, so a
    /// caller reads the state the call would change after that write before it calls again.
    pub fn kept_batch(&self) -> Option<[u8; 16]> {
        match self {
            Self::Client(error) => error.kept_batch(),
            _ => None,
        }
    }

    /// The refusal, when a rule of Paykit refused the call.
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }

    /// The SDK's own state in the store doesn't read.
    pub(crate) fn corrupt() -> Self {
        Self::Client(AppError::Store(StoreError::Corrupt))
    }

    /// Whether this is a value of the app's own state that doesn't read
    /// ([`PaykitError::corrupt`]).
    pub(crate) fn is_corrupt_state(&self) -> bool {
        matches!(self, Self::Client(AppError::Store(StoreError::Corrupt)))
    }
}

impl From<AppError> for PaykitError {
    fn from(error: AppError) -> Self {
        Self::Client(error)
    }
}

impl From<StoreError> for PaykitError {
    fn from(error: StoreError) -> Self {
        Self::Client(AppError::Store(error))
    }
}

impl From<paykit_lib::Error> for PaykitError {
    fn from(error: paykit_lib::Error) -> Self {
        Self::Lib(error)
    }
}

impl From<pubky_switch_core::Error> for PaykitError {
    fn from(error: pubky_switch_core::Error) -> Self {
        Self::Lib(paykit_lib::Error::Switch(error))
    }
}

impl From<RailError> for PaykitError {
    fn from(error: RailError) -> Self {
        Self::Rail(error)
    }
}

impl From<Refusal> for PaykitError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// The SDK's result type.
pub type Result<T> = std::result::Result<T, PaykitError>;

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_switch_client::error::ClientError;

    #[test]
    fn test_error_carries_the_switch_code() {
        let error = PaykitError::from(AppError::Switch(ClientError::Switch {
            status: 409,
            code: ErrorCode::Conflict,
            message: "scripted".into(),
            current: serde_json::Value::Null,
            known_version: None,
            retry_after: None,
        }));
        assert_eq!(error.code(), Some(ErrorCode::Conflict));
        assert_eq!(PaykitError::from(Refusal::Rejected).code(), None);
    }

    #[test]
    fn test_error_refusal_is_only_a_refusal() {
        let refused = PaykitError::from(Refusal::AlreadyPaid);
        assert_eq!(refused.refusal(), Some(&Refusal::AlreadyPaid));
        assert!(PaykitError::Conflict("record".into()).refusal().is_none());
        assert!(PaykitError::corrupt().refusal().is_none());
    }
}
