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
    /// The call needs a scope the app's certificate doesn't hold, such as `pay` to pay or
    /// `request` to send a request (Paykit spec 3.1).
    #[error("Paykit spec 3.1: the certificate doesn't hold {0}")]
    ScopeMissing(String),
    /// A labelled selling app sends every request under one of its labels, and only under a
    /// label it holds (Paykit spec 3.1, 8.1).
    #[error("Paykit spec 3.1: {0}")]
    Label(String),
    /// The request was rejected by this identity (Paykit spec 8.1).
    #[error("the request was rejected")]
    Rejected,
    /// Restored from a backup, the app holds the request: the payee may have cancelled it
    /// since the backup. It is paid only once the user has seen that
    /// ([`crate::PayOptions::possibly_cancelled_confirmed`]; Paykit spec 7.2, "What a
    /// restore loses").
    #[error("Paykit spec 7.2: held after a restore; the payee may have cancelled it")]
    HeldAfterRestore,
    /// The request was cancelled by either side (Paykit spec 8.1).
    #[error("the request was cancelled")]
    Cancelled,
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
    /// or an attempt a restore left unresolved. Only a user who has seen "may already be
    /// paid" pays it (Paykit spec 7.1, 7.2). `doubt`, when there is one, is what the user
    /// is shown and may pay over: paying anyway carries it back
    /// ([`crate::PayOptions::with_may_be_paid_confirmed`]), and only that same doubt is
    /// passed. `None` for a reason no confirmation passes, such as a pinned deadline other
    /// than the app's own, or a claim that doesn't show this app's attempt open.
    #[error("may already be paid: {reason}")]
    MayBePaid {
        /// The rule that makes it doubtful.
        reason: String,
        /// What the user may pay over.
        doubt: Option<Box<crate::PaymentDoubt>>,
    },
    /// The attempt's deadline has passed, or the request's proposal expired (Paykit spec 7.1).
    #[error("Paykit spec 7.1: the deadline has passed")]
    DeadlinePassed,
    /// A recurring request is paid per Billing Period, which this version of the SDK doesn't
    /// do yet (Paykit spec 8.2).
    #[error("Paykit spec 8.2: a recurring request is paid per Billing Period")]
    Recurring,
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
    /// No usable Private Payment List from the payee: none held, or each one expired or its
    /// single-use endpoints the payment would use already used (Paykit spec 8.4, "No usable
    /// list"). The app asked the payee for its current list with a
    /// `paykit.private_payment_list_request`; the payee can't be forced to answer, so the
    /// user is told the app is waiting for the payee's list. The user may choose to pay to
    /// the payee's public endpoints instead ([`crate::PaymentMode::Public`]); the app never
    /// switches on its own.
    #[error("Paykit spec 8.4: waiting for the payee's Private Payment List")]
    WaitingForList,
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
    /// The app's own switch doesn't answer. Paykit spec 8.4 lets a payment without a request
    /// to a reusable public endpoint go ahead without it; this SDK doesn't make one, and pays
    /// only attempt-first (Paykit spec 7.2). Nothing was written or paid.
    #[error("Paykit spec 8.4: the own switch doesn't answer: {0}")]
    SwitchUnavailable(String),
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
    /// The switch's error code, when the failure is an answer of the switch.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Client(error) => error.code(),
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
