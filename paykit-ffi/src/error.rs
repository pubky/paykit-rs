use paykit_sdk::{PaykitError, Refusal};
use pubky_switch_client::{
    app::{AppError, Refusal as AppRefusal},
    error::StoreError,
};
use std::sync::Arc;

/// A failure returned to the host without losing the original refusal or storage outcome.
#[derive(Debug, Clone, thiserror::Error, uniffi::Error)]
pub enum WalletError {
    /// An input supplied by the host failed validation.
    #[error("invalid {field}")]
    InvalidInput {
        /// Name of the invalid field, without its contents.
        field: String,
    },
    /// The source device must be in custody or known wiped before restoring.
    #[error("revoke the old app and enroll this device with new keys")]
    SourceDeviceUnaccountedFor,
    /// The SDK has no installed wallet under this App ID.
    #[error("no wallet with this app id")]
    NotJoined,
    /// The restored app awaits the host's answer about use after this backup.
    #[error("the restore question is open")]
    RestoreQuestionOpen,
    /// This enrollment reference has no pending request for the operation.
    #[error("the enrollment is absent or already complete")]
    EnrollmentUnavailable,
    /// The reference already belongs to another authentication request or approval.
    #[error("the enrollment is bound to another approval or authentication request")]
    EnrollmentConflict,
    /// The approval failed verification against the chosen identity and authentication request.
    #[error("approval verification failed")]
    ApprovalRefused,
    /// The native secure random generator could not produce the required key material.
    #[error("secure randomness unavailable")]
    RandomUnavailable,
    /// The protected store failed or another writer owns it.
    #[error("storage: {failure}")]
    Storage {
        /// The original storage failure, including an unknown commit outcome.
        failure: crate::StorageError,
    },
    /// The wallet handle was shut down.
    #[error("the wallet is closed")]
    Closed,
    /// A wallet call was made on the thread of a synchronous host callback, which the call
    /// would have waited for. A call from another thread the callback waits on can't be told
    /// apart from any other call: it waits for the callback, and the wallet hangs.
    #[error("a wallet call from inside a host callback")]
    Reentrant,
    /// The payment method can't look up a payment by its attempt ID, so a payment the wallet
    /// stopped waiting for could never be settled: paying is refused. Every other call works.
    #[error("the payment method can't look up a payment by its attempt ID")]
    PaymentMethodWithoutLookup,
    /// The bindings could not start the threads they run wallet work on.
    #[error("the wallet runtime is unavailable")]
    RuntimeUnavailable,
    /// The user must see this and decide. A held, blocked, may-be-paid, transfer or rate
    /// refusal is confirmed by passing the same `failure.refusal` back in
    /// `PaymentOptions.confirmed_refusals`; every other code names the call that answers it
    /// (the host guide's "Wallet errors" table).
    #[error("needs the user: {}", failure.code)]
    NeedsUser {
        /// What failed and the refusal to show.
        failure: Arc<WalletFailure>,
    },
    /// Nothing is wrong that waiting won't fix: the network, the service, or a call that can't
    /// run now. Try again later, after `failure.retry_after_ms` when it is set.
    #[error("retry later: {}", failure.code)]
    RetryLater {
        /// What failed.
        failure: Arc<WalletFailure>,
    },
    /// The call may have taken effect. Reconcile before trying again; `failure.kept_batch`
    /// names a Switch batch kept for this call.
    #[error("outcome unknown: {}", failure.code)]
    OutcomeUnknown {
        /// What failed.
        failure: Arc<WalletFailure>,
    },
    /// Nothing took effect: the call was refused before anything was written or applied, and
    /// is safe to make again once its cause is fixed.
    #[error("not sent: {}", failure.code)]
    NotSent {
        /// What failed.
        failure: Arc<WalletFailure>,
    },
    /// The answer is final: the request was rejected, cancelled or already paid, or the
    /// refusal is one the bindings don't recognize (`unknown`). Trying again gives the same
    /// answer, and none of these is confirmed.
    #[error("permanent: {}", failure.code)]
    Permanent {
        /// What failed.
        failure: Arc<WalletFailure>,
    },
    /// The wallet stopped for a recovery: an alarm, a recovery notice, a certificate Paykit
    /// refused, or a held record. Show it and follow the recovery steps.
    #[error("recovery: {}", failure.code)]
    Recovery {
        /// What failed.
        failure: Arc<WalletFailure>,
    },
}

/// What failed, for a host that branches on `code` and shows or reports the rest. Codes are
/// stable snake_case names, fixed with the first packaged release to a host.
#[derive(Debug, uniffi::Object)]
pub struct WalletFailure {
    pub(crate) code: String,
    pub(crate) context: String,
    pub(crate) service_code: Option<String>,
    pub(crate) retry_after_ms: Option<u64>,
    pub(crate) kept_batch: Option<Vec<u8>>,
    pub(crate) switch_refusal: Option<Arc<crate::SwitchRefusal>>,
    pub(crate) refusal: Option<Arc<PaymentRefusal>>,
}

#[uniffi::export]
impl WalletFailure {
    /// What failed, as a stable machine-readable name.
    pub fn code(&self) -> String {
        self.code.clone()
    }
    /// A redacted explanation for logs and support: it names the rule, never wallet data.
    pub fn context(&self) -> String {
        self.context.clone()
    }
    /// The Switch service's error code, when it answered with one.
    pub fn service_code(&self) -> Option<String> {
        self.service_code.clone()
    }
    /// When to try again, in milliseconds, when the service said.
    pub fn retry_after_ms(&self) -> Option<u64> {
        self.retry_after_ms
    }
    /// A Switch batch kept for this call, which the wallet sends again by itself.
    pub fn kept_batch(&self) -> Option<Vec<u8>> {
        self.kept_batch.clone()
    }
    /// The Switch rule that held this call or a batch ahead of it.
    pub fn switch_refusal(&self) -> Option<Arc<crate::SwitchRefusal>> {
        self.switch_refusal.clone()
    }
    /// The payment refusal behind the failure, with its evidence.
    pub fn refusal(&self) -> Option<Arc<PaymentRefusal>> {
        self.refusal.clone()
    }
}

impl WalletError {
    pub(crate) fn invalid(s: &str) -> Self {
        Self::InvalidInput { field: s.into() }
    }
}

/// The action a failure calls for: the `WalletError` case it comes as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Category {
    NeedsUser,
    RetryLater,
    OutcomeUnknown,
    NotSent,
    Permanent,
    Recovery,
}

impl Category {
    /// The `WalletError` case this category comes as.
    #[cfg(test)]
    pub(crate) fn case(self) -> &'static str {
        match self {
            Category::NeedsUser => "NeedsUser",
            Category::RetryLater => "RetryLater",
            Category::OutcomeUnknown => "OutcomeUnknown",
            Category::NotSent => "NotSent",
            Category::Permanent => "Permanent",
            Category::Recovery => "Recovery",
        }
    }
}

fn failure(category: Category, code: &str, context: String) -> (Category, WalletFailure) {
    (
        category,
        WalletFailure {
            code: code.into(),
            context,
            service_code: None,
            retry_after_ms: None,
            kept_batch: None,
            switch_refusal: None,
            refusal: None,
        },
    )
}

fn into_error((category, failure): (Category, WalletFailure)) -> WalletError {
    let failure = Arc::new(failure);
    match category {
        Category::NeedsUser => WalletError::NeedsUser { failure },
        Category::RetryLater => WalletError::RetryLater { failure },
        Category::OutcomeUnknown => WalletError::OutcomeUnknown { failure },
        Category::NotSent => WalletError::NotSent { failure },
        Category::Permanent => WalletError::Permanent { failure },
        Category::Recovery => WalletError::Recovery { failure },
    }
}

/// The action and stable code of each payment refusal, the table of the host guide's
/// "Wallet errors" section. Every kind the bindings show has a case. `Unknown`, a refusal the
/// bindings don't recognize, is final: never retried and never confirmed.
pub(crate) fn refusal_category(kind: &crate::RefusalKind) -> (Category, &'static str) {
    use crate::RefusalKind as K;
    use Category::*;
    match kind {
        // Confirmed by passing the same refusal back (`PaymentOptions.confirmed_refusals`).
        K::HeldAfterRestore { .. } => (NeedsUser, "held_after_restore"),
        K::Blocked { .. } => (NeedsUser, "blocked"),
        K::MayBePaid { doubt: Some(_) } => (NeedsUser, "may_be_paid"),
        // No doubt to confirm: the user settles it with `reconcile` first.
        K::MayBePaid { doubt: None } => (NeedsUser, "may_be_paid_reconcile"),
        K::Transfer { .. } => (NeedsUser, "transfer"),
        K::PricedByRate { .. } => (NeedsUser, "priced_by_rate"),
        // Decided by the user through a call of its own.
        K::LossChanged { .. } => (NeedsUser, "loss_changed"),
        K::ListDeliveryUnknown { .. } => (NeedsUser, "list_delivery_unknown"),
        K::Divergent { .. } => (NeedsUser, "divergent"),
        K::MaybeCredited => (NeedsUser, "maybe_credited"),
        K::SettlementBucketFull { .. } => (NeedsUser, "settlement_bucket_full"),
        K::PeriodNotStarted => (NeedsUser, "period_not_started"),
        K::IdentityUnconfirmed => (NeedsUser, "identity_unconfirmed"),
        K::ScopeMissing { .. } => (NeedsUser, "scope_missing"),
        K::NotSubscribed => (NeedsUser, "not_subscribed"),
        // The SDK gives its reason as text only, and the reasons span every case, from a wait
        // to an alarm: shown to the user, it is never retried by itself.
        K::NotNow => (NeedsUser, "not_now"),
        K::WaitingForList {
            device_clock_behind: false,
        } => (RetryLater, "waiting_for_list"),
        // The list expired only by the service's time: the device clock is behind it.
        K::WaitingForList {
            device_clock_behind: true,
        } => (NeedsUser, "waiting_for_list_clock_behind"),
        K::SwitchUnavailable => (RetryLater, "switch_unavailable"),
        K::PaymentInProgress => (RetryLater, "payment_in_progress"),
        K::BeingHandledByOtherWallet => (RetryLater, "being_handled_by_other_wallet"),
        K::IdentityUnavailable => (RetryLater, "identity_unavailable"),
        K::AttemptedPeriodsFull => (RetryLater, "attempted_periods_full"),
        // Refused before anything was written.
        K::NoDestination => (NotSent, "no_destination"),
        K::UnsupportedEndpoint => (NotSent, "unsupported_endpoint"),
        K::Destination => (NotSent, "destination"),
        K::Label => (NotSent, "label"),
        K::Recurring => (NotSent, "recurring"),
        K::Usage => (NotSent, "usage"),
        // The attempt was written and then failed before money moved: it is closed. A
        // failure with its cause comes as the cause's case.
        K::AttemptFailed => (Permanent, "attempt_failed"),
        K::NotFound => (Permanent, "not_found"),
        K::Rejected => (Permanent, "rejected"),
        K::Cancelled => (Permanent, "cancelled"),
        K::AllowanceEnded => (Permanent, "allowance_ended"),
        K::AllowanceConflicted => (Permanent, "allowance_conflicted"),
        K::CreditHistoryLost => (Permanent, "credit_history_lost"),
        K::AlreadyPaid => (Permanent, "already_paid"),
        K::DeadlinePassed => (Permanent, "deadline_passed"),
        K::PaymentTimeOutside { .. } => (Permanent, "payment_time_outside"),
        K::Subscribed => (Permanent, "subscribed"),
        K::ManualOnly => (Permanent, "manual_only"),
        // Final for automatic paying; the user can still pay by hand.
        K::NotAutomatic => (Permanent, "not_automatic"),
        K::Unknown => (Permanent, "unknown"),
        K::NotCurrent => (Recovery, "not_current"),
        K::RecordHeld => (Recovery, "record_held"),
        K::CertificateRefused => (Recovery, "certificate_refused"),
    }
}

/// The action a Switch service's error answer calls for, by the Retry column of Switch spec
/// 6.4. The code is `switch_` and the wire's error code (Switch wire 7.6).
fn service_category(code: pubky_switch_core::http::ErrorCode) -> (Category, String) {
    use pubky_switch_core::http::{ErrorCode as E, Retry as R};
    use Category::*;
    let category = match (code, code.retry()) {
        // The app's certificate or its place in the identity ended: stop and recover.
        (E::NotCurrent | E::CertExpired, _) | (_, R::HeldForNextGeneration) => Recovery,
        (_, R::No) => Permanent,
        // The state the call was computed from changed: nothing applied, decide again.
        (_, R::AfterReread) => NotSent,
        // The same call can succeed once time passes or the client has caught up.
        (
            _,
            R::WithNewTimestamp
            | R::AfterKeyBundle
            | R::AfterResigning
            | R::AfterResealing
            | R::AfterDeliveringIdentity
            | R::AtNextService
            | R::AfterRetryAfter
            | R::WhenLimitFrees
            | R::Yes,
        ) => RetryLater,
    };
    (category, format!("switch_{}", code.as_str()))
}

/// The action and stable code of a failure of the Switch client's base client.
fn switch_client_category(e: &pubky_switch_client::error::ClientError) -> (Category, String) {
    use pubky_switch_client::error::ClientError as C;
    use Category::*;
    let (category, code) = match e {
        C::Switch { code, .. } => return service_category(*code),
        C::OwnRecoveryRequired { .. } => (Recovery, "own_recovery_required"),
        // Signed by the service and not verifying: evidence handled as a rollback.
        C::Chain(_) => (Recovery, "chain"),
        C::DisplayReadPending => (RetryLater, "display_read_pending"),
        C::Transport(_) => (RetryLater, "transport"),
        C::BadServiceSignature { .. } => (RetryLater, "bad_service_signature"),
        C::StaleAnswer { .. } => (RetryLater, "stale_answer"),
        C::BodyUnread { .. } => (RetryLater, "body_unread"),
        C::InvalidResponse(_) => (RetryLater, "invalid_response"),
        C::BodyOverCap { .. } => (NotSent, "body_over_cap"),
        C::Unpinned => (NotSent, "unpinned"),
        C::Usage(_) => (NotSent, "client_usage"),
        C::Core(_) => (Permanent, "invalid_data"),
        _ => (OutcomeUnknown, "client_unclassified"),
    };
    (category, code.into())
}

/// The action and stable code of a rule of the Switch client's app layer that refused a call.
pub(crate) fn app_refusal_category(refusal: &AppRefusal) -> (Category, &'static str) {
    use Category::*;
    match refusal {
        AppRefusal::Stopped => (Recovery, "stopped"),
        AppRefusal::Left => (Recovery, "left"),
        AppRefusal::Gone => (Recovery, "gone"),
        AppRefusal::RecoveryRequired { .. } => (Recovery, "recovery_required"),
        AppRefusal::Certificate { .. } => (Recovery, "certificate_refused"),
        AppRefusal::Approval(_) => (Permanent, "approval_refused"),
        AppRefusal::NotJoined => (Permanent, "not_joined"),
        AppRefusal::RestoreQuestionOpen => (NeedsUser, "restore_question_open"),
        // Text only, from reconcile mode to the clocks: shown, never retried by itself.
        AppRefusal::NotNow(_) => (NeedsUser, "client_not_now"),
        AppRefusal::KeptBatchesAhead(_) => (RetryLater, "kept_batches_ahead"),
        AppRefusal::Usage(_) => (NotSent, "client_usage"),
        AppRefusal::CarriedTooLarge { .. } => (NotSent, "carried_too_large"),
        AppRefusal::WontDeliver(_) => (Permanent, "wont_deliver"),
        _ => (OutcomeUnknown, "client_unclassified"),
    }
}

/// The action, code and Switch details of a failure of the Switch client.
fn client_failure(e: &AppError) -> (Category, WalletFailure) {
    use pubky_switch_client::error::ClientError;
    use Category::*;
    let context = e.to_string();
    let (mut category, mut failure) = match e.unkept() {
        AppError::ExpiredUnsent => failure(NotSent, "expired_unsent", context),
        AppError::Transport(_) => failure(RetryLater, "transport", context),
        AppError::Protocol(_) => failure(RetryLater, "protocol_store", context),
        AppError::NotAnsweringAsAccount { .. } => {
            failure(RetryLater, "account_not_answering", context)
        }
        AppError::InvalidData(_) => failure(Permanent, "invalid_data", context),
        AppError::Alarm(_) => failure(Recovery, "alarm", context),
        AppError::Refused(refusal) => {
            let (category, code) = app_refusal_category(refusal);
            failure(category, code, context)
        }
        AppError::Switch(switch) => {
            let (category, code) = switch_client_category(switch);
            let (category, mut failure) = failure(category, &code, context);
            if let ClientError::Switch { retry_after, .. }
            | ClientError::BodyUnread { retry_after, .. } = switch
            {
                failure.retry_after_ms = retry_after.map(|s| s.saturating_mul(1000));
            }
            (category, failure)
        }
        _ => failure(OutcomeUnknown, "client_unclassified", context),
    };
    failure.service_code = e.code().map(|code| code.as_str().into());
    failure.kept_batch = e.kept_batch().map(Vec::from);
    failure.switch_refusal = crate::SwitchRefusal::from_error(e);
    // A batch kept for this call may have applied: reconcile before trying again.
    if failure.kept_batch.is_some() && !matches!(category, Recovery) {
        category = OutcomeUnknown;
    }
    (category, failure)
}

impl From<StoreError> for WalletError {
    fn from(e: StoreError) -> Self {
        use crate::StorageError as S;
        let failure = match e {
            StoreError::InUse => S::InUse,
            StoreError::NotOwner => S::NotOwner,
            StoreError::Unavailable => S::Unavailable,
            StoreError::Unknown => S::Unknown,
            StoreError::Full => S::Full,
            StoreError::Corrupt => S::Corrupt,
            StoreError::TooLarge { size, cap, .. } => S::TooLarge { size, cap },
            _ => S::Other,
        };
        Self::Storage { failure }
    }
}
impl From<AppError> for WalletError {
    fn from(e: AppError) -> Self {
        PaykitError::Client(e).into()
    }
}
impl From<PaykitError> for WalletError {
    fn from(e: PaykitError) -> Self {
        use Category::*;
        let context = e.to_string();
        let categorized = match &e {
            PaykitError::Refused(r) => {
                let refusal = Arc::new(PaymentRefusal(r.clone()));
                let (mut category, code) = refusal_category(&refusal.kind());
                // The attempt was closed and nothing paid: what to do next is its cause's.
                if let paykit_sdk::Refusal::AttemptFailed(cause) = r {
                    category = refusal_category(&PaymentRefusal((**cause).clone()).kind()).0;
                }
                let (category, mut failure) = failure(category, code, context);
                failure.refusal = Some(refusal);
                (category, failure)
            }
            PaykitError::Client(AppError::Refused(
                pubky_switch_client::app::Refusal::NotJoined,
            )) => return Self::NotJoined,
            PaykitError::Client(AppError::Refused(
                pubky_switch_client::app::Refusal::RestoreQuestionOpen,
            )) => return Self::RestoreQuestionOpen,
            PaykitError::Client(AppError::Store(e)) => return e.clone().into(),
            PaykitError::Client(AppError::Protocol(failure)) => {
                return failure.error().clone().into()
            }
            PaykitError::Client(error) => client_failure(error),
            // The edit is saved and published once what stopped its signing clears, unless
            // that is a recovery.
            PaykitError::PublicEndpointChangePending(error) => {
                let (category, mut failure) = client_failure(error);
                failure.code = "public_endpoint_change_pending".into();
                let category = match category {
                    Recovery => Recovery,
                    _ => RetryLater,
                };
                (category, failure)
            }
            PaykitError::Rail(error) => match error {
                paykit_sdk::RailError::Unavailable(_) => {
                    failure(RetryLater, "method_unknown", context)
                }
                paykit_sdk::RailError::Refused(_) => {
                    failure(NotSent, "method_not_submitted", context)
                }
                paykit_sdk::RailError::InvalidSelection(_) => {
                    failure(Permanent, "method_invalid_selection", context)
                }
                _ => failure(OutcomeUnknown, "method_unclassified", context),
            },
            // A value the caller gave, against data from the network or the store.
            PaykitError::Lib(paykit_lib::Error::Validation(_)) => {
                failure(NotSent, "invalid_input", context)
            }
            PaykitError::Lib(_) => failure(Permanent, "invalid_protocol_data", context),
            // Nothing applied: read the current state, decide again, then redo.
            PaykitError::Conflict(_) => failure(NotSent, "conflict", context),
            PaykitError::Homeserver(_) => failure(RetryLater, "homeserver", context),
            _ => failure(OutcomeUnknown, "unclassified", context),
        };
        into_error(categorized)
    }
}

/// The exact payment refusal and its confirmation evidence retained by the SDK.
#[derive(Debug, uniffi::Object)]
pub struct PaymentRefusal(pub(crate) Refusal);

#[uniffi::export]
impl PaymentRefusal {
    /// Return the native explanation for display.
    pub fn reason(&self) -> String {
        self.0.to_string()
    }
    /// For a failed earlier attempt, return the underlying reason when available.
    pub fn attempted_failure(&self) -> Option<Arc<Self>> {
        match &self.0 {
            Refusal::AttemptFailed(r) => Some(Arc::new(Self(*r.clone()))),
            _ => None,
        }
    }
}
