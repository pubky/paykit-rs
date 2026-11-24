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
    /// `PaymentOptions.confirmed_refusals`, and an `attempt_failed` one by passing its
    /// `attempted_failure()`; every other code names the call that answers it (the host
    /// guide's "Wallet errors" table).
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
    /// No payment was made and no message went out: the call is safe to make again once its
    /// cause is fixed. An attempt's own writes may have happened: an `attempt_failed` stores
    /// its attempt, closed as failed, and may mark a single-use endpoint used, which a retry
    /// then leaves out.
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
    /// The wallet as a whole stopped for a recovery: an alarm, a recovery notice, a
    /// certificate Paykit refused, or its authority. Show it and follow the recovery steps.
    /// A hold on one record is `NeedsUser`, never this.
    #[error("recovery: {}", failure.code)]
    Recovery {
        /// What failed.
        failure: Arc<WalletFailure>,
    },
}

/// What failed, for a host that branches on `code` and shows or reports the rest. Codes are
/// stable snake_case names, fixed with the first packaged release to a host.
#[derive(uniffi::Object)]
pub struct WalletFailure {
    pub(crate) code: String,
    pub(crate) context: String,
    pub(crate) service_code: Option<String>,
    pub(crate) retry_after_ms: Option<u64>,
    pub(crate) quota_limit: Option<String>,
    pub(crate) settlement_bucket: Option<u16>,
    pub(crate) settlement_warning: Option<SettlementWarningOutcome>,
    pub(crate) kept_batch: Option<Vec<u8>>,
    pub(crate) switch_refusal: Option<Arc<crate::SwitchRefusal>>,
    pub(crate) refusal: Option<Arc<PaymentRefusal>>,
}

/// The fields a host may log; the refusals show only their codes.
impl std::fmt::Debug for WalletFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletFailure")
            .field("code", &self.code)
            .field("service_code", &self.service_code)
            .field("retry_after_ms", &self.retry_after_ms)
            .field("quota_limit", &self.quota_limit)
            .field("settlement_bucket", &self.settlement_bucket)
            .field("settlement_warning", &self.settlement_warning)
            .field("kept_batch", &self.kept_batch.is_some())
            .field("switch_refusal", &self.switch_refusal)
            .field("refusal", &self.refusal)
            .finish()
    }
}

#[uniffi::export]
impl WalletFailure {
    /// What failed, as a stable machine-readable name.
    pub fn code(&self) -> String {
        self.code.clone()
    }
    /// A fixed explanation for logs and support: the code and what its case means, never
    /// text the service, the network, the payment method, the store or a document gave.
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
    /// The budget a full quota names (`quota_exceeded`'s `current.limit`, Switch wire 9.3),
    /// such as `storage` or `contacts`, so the host can say which one to clear; `None` for a
    /// name these bindings don't know.
    pub fn quota_limit(&self) -> Option<String> {
        self.quota_limit.clone()
    }
    /// For `settlement_warning_unsaved`, the settlement bucket whose record slot the Switch
    /// refused: the host shows that bucket's warning if it has yet to.
    pub fn settlement_bucket(&self) -> Option<u16> {
        self.settlement_bucket
    }
    /// For `settlement_warning_unsaved`, what became of the bucket's warning in the store:
    /// not saved, or saved or not, unknown. `None` for every other code.
    pub fn settlement_warning_outcome(&self) -> Option<SettlementWarningOutcome> {
        self.settlement_warning
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

    /// The failure of the six cases; `None` for the enrollment, restore and store answers.
    #[cfg(any(test, feature = "test-wallet"))]
    pub(crate) fn failure(&self) -> Option<&Arc<WalletFailure>> {
        match self {
            Self::NeedsUser { failure }
            | Self::RetryLater { failure }
            | Self::OutcomeUnknown { failure }
            | Self::NotSent { failure }
            | Self::Permanent { failure }
            | Self::Recovery { failure } => Some(failure),
            _ => None,
        }
    }
}

/// The Switch client's chain proof failing: handled as a rollback alarm.
const CHAIN: (Category, &str) = (Category::Recovery, "chain");
/// A write kept unsent behind earlier kept batches: the client returns it only with the
/// call's own batch kept, so it may still apply.
const KEPT_BATCHES_AHEAD: (Category, &str) = (Category::OutcomeUnknown, "kept_batches_ahead");
/// A Switch client failure these bindings don't know.
const CLIENT_UNCLASSIFIED: (Category, &str) = (Category::OutcomeUnknown, "client_unclassified");
/// A payment method failure these bindings don't know.
const METHOD_UNCLASSIFIED: (Category, &str) = (Category::OutcomeUnknown, "method_unclassified");
/// A Paykit failure these bindings don't know.
const UNCLASSIFIED: (Category, &str) = (Category::OutcomeUnknown, "unclassified");

/// The codes whose failures no test can build: the mapping's own arms, so the host guide's
/// table is checked against them as they are.
#[cfg(test)]
pub(crate) const UNBUILT: [(Category, &str); 5] = [
    CHAIN,
    KEPT_BATCHES_AHEAD,
    CLIENT_UNCLASSIFIED,
    METHOD_UNCLASSIFIED,
    UNCLASSIFIED,
];

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
    /// What the case tells the host, in fixed words: a failure's `context` is built from its
    /// code and this alone, never from text the service, the network, the payment method, the
    /// store or a document gave.
    fn meaning(self) -> &'static str {
        match self {
            Category::NeedsUser => "the user must see this and decide",
            Category::RetryLater => "a later call can succeed",
            Category::OutcomeUnknown => "the call may have taken effect",
            Category::NotSent => "no payment or message went out",
            Category::Permanent => "the answer is final",
            Category::Recovery => "the wallet stopped for a recovery",
        }
    }

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

fn failure(category: Category, code: &str) -> (Category, WalletFailure) {
    (
        category,
        WalletFailure {
            code: code.into(),
            context: String::new(),
            service_code: None,
            retry_after_ms: None,
            quota_limit: None,
            settlement_bucket: None,
            settlement_warning: None,
            kept_batch: None,
            switch_refusal: None,
            refusal: None,
        },
    )
}

fn into_error((category, mut failure): (Category, WalletFailure)) -> WalletError {
    // Set last, from the final code and case alone.
    failure.context = format!("{}: {}", failure.code, category.meaning());
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
        // No wait lifts it, only the shared post-loss hold standing too; the user pays by hand.
        K::FloorHeld => (NeedsUser, "floor_held"),
        // Confirmed with `confirm_allowance_executor` once the user has seen the Allowance.
        K::AllowanceConfirmationNeeded { .. } => (NeedsUser, "allowance_confirmation_needed"),
        // The user answers first, or is shown a hold on one record; every reason of this
        // refusal comes as `NeedsUser`.
        K::HeldForUser { reason } => (NeedsUser, held_for_user_code(reason)),
        // A wait: every reason of this refusal comes as `RetryLater`.
        K::NotNow { reason } => (RetryLater, not_now_code(reason)),
        // A recovery step of the wallet as a whole: every reason comes as `Recovery`.
        K::RecoveryNeeded { reason } => (Recovery, recovery_code(reason)),
        K::WaitingForList {
            device_clock_behind: false,
        } => (RetryLater, "waiting_for_list"),
        // The list expired only by the service's time: the device clock is behind it, and the
        // user is told so (Paykit spec 8.4), even if a newer list clears it later.
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
        // One record is held, not the wallet: shown for that request, with nothing to retry.
        // It reads again or is written again, or, with its payment history lost to a delete,
        // holds that obligation for good; the refusal doesn't say which yet.
        K::RecordHeld => (NeedsUser, "record_held"),
        K::CertificateRefused => (Recovery, "certificate_refused"),
    }
}

/// The stable code of each wait.
fn not_now_code(reason: &crate::NotNowKind) -> &'static str {
    use crate::NotNowKind as R;
    match reason {
        R::RecordsUnlisted { .. } => "records_unlisted",
        R::RecordsNotListed => "records_not_listed",
        R::ScopeKeysAwaited => "scope_keys_awaited",
        R::ValueAwaitsKey => "value_awaits_key",
        R::FloorUnreadable => "floor_unreadable",
        R::FloorRaiseWaiting => "floor_raise_waiting",
        R::NoServiceTime => "no_service_time",
        R::ReadAgain => "read_again",
        R::EarlierWriteUnresolved => "earlier_write_unresolved",
        R::CallAgain => "call_again",
        R::AllowanceLimit { .. } => "allowance_limit",
        R::Unknown => "not_now",
    }
}

/// The stable code of each answer the user gives first.
fn held_for_user_code(reason: &crate::HeldForUserKind) -> &'static str {
    use crate::HeldForUserKind as R;
    match reason {
        R::CounterpartyBlocked => "counterparty_blocked",
        R::HeldFromAutomatic => "held_from_automatic",
        R::ActivityBaseChanged => "activity_base_changed",
        R::CancellationOrderUnknown => "cancellation_order_unknown",
        R::PublicationConflict => "publication_conflict",
        R::RecordUnreadable => "record_unreadable",
        R::AllowanceNotResumed => "allowance_not_resumed",
        R::AllowanceExecutionHeld => "allowance_execution_held",
        R::AttemptCheck => "attempt_check",
        R::ReloadRequired => "reload_required",
        R::SwitchClient => "switch_client_hold",
        R::Unresolved => "held_unresolved",
        R::PendingProposalsFull => "pending_proposals_full",
        R::AllowanceLedgerFull => "allowance_ledger_full",
        R::AnswerOutcomeUnknown => "answer_outcome_unknown",
        R::Unknown => "held_for_user",
    }
}

/// The stable code of each recovery step.
fn recovery_code(reason: &crate::RecoveryNeededKind) -> &'static str {
    use crate::RecoveryNeededKind as R;
    match reason {
        R::PayerPassOwed => "payer_pass_owed",
        R::Unknown => "recovery_needed",
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
        // The certificate lacks the permission: the user approves a wider one.
        (E::PermissionMissing, _) => NeedsUser,
        // The identity.json delivered isn't the newest: a receive reads the current one.
        (E::StaleIdentity, _) => RetryLater,
        // Nothing applied: the recipient may admit the sender later, or a smaller request
        // fits.
        (E::NotAdmitted | E::TooLarge, _) => NotSent,
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

/// A limit name of Switch wire 9.3, as the bindings' own text: a name the answer gives that
/// isn't one of them is never passed on, since the answer's text is the service's.
fn known_quota_limit(limit: &str) -> Option<&'static str> {
    [
        "inbox",
        "sender_share",
        "storage",
        "record_keys",
        "claims",
        "claim_share",
        "attempts",
        "open_attempts",
        "contacts",
        "conversations",
        "deny",
        "invite_codes",
        "connect_requests",
    ]
    .into_iter()
    .find(|known| *known == limit)
}

/// The action a full quota calls for, by the limit the answer names and what frees it
/// (Switch wire 9.3).
fn quota_category(limit: Option<&str>) -> (Category, String) {
    use Category::*;
    let (category, code) = match limit {
        // Frees with time: the recipient's apps reading their feed, or the service deleting
        // old claims.
        Some("inbox" | "sender_share" | "claims" | "claim_share") => {
            (RetryLater, "switch_quota_wait")
        }
        // Frees as this app's open attempts resolve, some only by the user's answer.
        Some("open_attempts") => (NeedsUser, "switch_quota_open_attempts"),
        // Frees only after the user removes something, through another app or the signer.
        Some(
            "storage" | "record_keys" | "contacts" | "conversations" | "deny" | "invite_codes",
        ) => (NeedsUser, "switch_quota_full"),
        // Nothing applied; the recipient may answer its pending requests, or never.
        Some("connect_requests") => (NotSent, "switch_quota_connect_requests"),
        // Final for this obligation: no more attempts fit its claim.
        Some("attempts") => (Permanent, "switch_quota_final"),
        // A limit these bindings don't know: shown, never retried by itself.
        _ => (NeedsUser, "switch_quota_exceeded"),
    };
    (category, code.into())
}

/// The action and stable code of a failure of the Switch client's base client.
fn switch_client_category(e: &pubky_switch_client::error::ClientError) -> (Category, String) {
    use pubky_switch_client::error::ClientError as C;
    use Category::*;
    let (category, code) = match e {
        C::Switch {
            code: pubky_switch_core::http::ErrorCode::QuotaExceeded,
            current,
            ..
        } => return quota_category(current.get("limit").and_then(|limit| limit.as_str())),
        C::Switch { code, .. } => return service_category(*code),
        C::OwnRecoveryRequired { .. } => (Recovery, "own_recovery_required"),
        // Signed by the service and not verifying: evidence handled as a rollback.
        C::Chain(_) => CHAIN,
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
        _ => CLIENT_UNCLASSIFIED,
    };
    (category, code.into())
}

/// The action and stable code of a rule of the Switch client's app layer that refused a call.
fn app_refusal_category(refusal: &AppRefusal) -> (Category, &'static str) {
    use Category::*;
    match refusal {
        AppRefusal::Stopped => (Recovery, "stopped"),
        AppRefusal::Left => (Recovery, "left"),
        AppRefusal::Gone => (Recovery, "gone"),
        AppRefusal::RecoveryRequired { .. } => (Recovery, "recovery_required"),
        AppRefusal::Certificate { .. } => (Recovery, "certificate_refused"),
        AppRefusal::Approval(_) => (Permanent, "approval_refused"),
        // Text only, from reconcile mode to the clocks: shown, never retried by itself.
        AppRefusal::NotNow(_) => (NeedsUser, "client_not_now"),
        AppRefusal::KeptBatchesAhead(_) => KEPT_BATCHES_AHEAD,
        AppRefusal::Usage(_) => (NotSent, "client_usage"),
        AppRefusal::CarriedTooLarge { .. } => (NotSent, "carried_too_large"),
        AppRefusal::DutiesTooLarge { .. } => (NotSent, "duties_too_large"),
        // The user unblocks enough peers to make room, then the recovery is followed again.
        AppRefusal::DenyListFull { .. } => (NeedsUser, "deny_list_full"),
        AppRefusal::WontDeliver(_) => (Permanent, "wont_deliver"),
        _ => CLIENT_UNCLASSIFIED,
    }
}

/// The action, code and Switch details of a failure of the Switch client.
fn client_failure(e: &AppError) -> (Category, WalletFailure) {
    use pubky_switch_client::error::ClientError;
    use Category::*;
    let (mut category, mut failure) = match e.unkept() {
        AppError::ExpiredUnsent => failure(NotSent, "expired_unsent"),
        // Reached only behind a kept batch: the commit failed after the batch was kept.
        AppError::Store(_) => failure(OutcomeUnknown, "store_after_kept"),
        AppError::Protocol(_) => failure(OutcomeUnknown, "protocol_store"),
        AppError::Transport(_) => failure(RetryLater, "transport"),
        AppError::NotAnsweringAsAccount { .. } => failure(RetryLater, "account_not_answering"),
        AppError::InvalidData(_) => failure(Permanent, "invalid_data"),
        AppError::Alarm(_) => failure(Recovery, "alarm"),
        AppError::Refused(refusal) => {
            let (category, code) = app_refusal_category(refusal);
            failure(category, code)
        }
        AppError::Switch(switch) => {
            let (category, code) = switch_client_category(switch);
            let (category, mut failure) = failure(category, &code);
            if let ClientError::Switch { retry_after, .. }
            | ClientError::BodyUnread { retry_after, .. } = switch
            {
                failure.retry_after_ms = retry_after.map(|s| s.saturating_mul(1000));
            }
            if let ClientError::Switch {
                code: pubky_switch_core::http::ErrorCode::QuotaExceeded,
                current,
                ..
            } = switch
            {
                failure.quota_limit = current
                    .get("limit")
                    .and_then(|limit| limit.as_str())
                    .and_then(known_quota_limit)
                    .map(Into::into);
            }
            (category, failure)
        }
        _ => failure(CLIENT_UNCLASSIFIED.0, CLIENT_UNCLASSIFIED.1),
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
        let categorized = match &e {
            PaykitError::Refused(r) => {
                let refusal = Arc::new(PaymentRefusal(r.clone()));
                let (category, code) = refusal.case();
                let (category, mut failure) = failure(category, code);
                failure.refusal = Some(refusal);
                (category, failure)
            }
            // The enrollment and restore answers keep their own cases, kept or not. A store
            // failure keeps its own case only bare: after a batch was kept it may have applied.
            PaykitError::Client(AppError::Store(e)) => return e.clone().into(),
            PaykitError::Client(AppError::Protocol(failure)) => {
                return failure.error().clone().into()
            }
            PaykitError::Client(error) => match error.unkept() {
                AppError::Refused(AppRefusal::NotJoined) => return Self::NotJoined,
                AppError::Refused(AppRefusal::RestoreQuestionOpen) => {
                    return Self::RestoreQuestionOpen
                }
                _ => client_failure(error),
            },
            // The edit is saved and published once what stopped its signing clears. The SDK
            // builds this from a refusal of the Switch client: a recovery stays one, kept
            // batches ahead keep their doubt, and anything else needs the user or a sibling
            // app to act, since the saved edit means "not sent" or "final" is untrue.
            PaykitError::PublicEndpointChangePending(error) => {
                let (category, mut failure) = client_failure(error);
                failure.code = "public_endpoint_change_pending".into();
                let category = match category {
                    Recovery => Recovery,
                    OutcomeUnknown => OutcomeUnknown,
                    RetryLater => RetryLater,
                    NeedsUser | NotSent | Permanent => NeedsUser,
                };
                (category, failure)
            }
            PaykitError::Rail(error) => match error {
                paykit_sdk::RailError::Unavailable(_) => failure(RetryLater, "method_unknown"),
                paykit_sdk::RailError::Refused(_) => failure(NotSent, "method_not_submitted"),
                paykit_sdk::RailError::InvalidSelection(_) => {
                    failure(Permanent, "method_invalid_selection")
                }
                _ => failure(METHOD_UNCLASSIFIED.0, METHOD_UNCLASSIFIED.1),
            },
            // A value the caller gave, against data from the network or the store.
            PaykitError::Lib(paykit_lib::Error::Validation(_)) => failure(NotSent, "invalid_input"),
            PaykitError::Lib(_) => failure(Permanent, "invalid_protocol_data"),
            // Nothing applied: read the current state, decide again, then redo.
            PaykitError::Conflict(_) => failure(NotSent, "conflict"),
            PaykitError::Homeserver(_) => failure(RetryLater, "homeserver"),
            // The Switch refused a new settlement bucket for lack of a record slot, and its
            // warning couldn't be saved or checked. The marked receipt and its delivery are
            // still kept, and the kept batch may still apply: the user is shown the bucket's
            // warning, which grants no consent to issue another receipt.
            PaykitError::SettlementNoticeUnavailable {
                bucket,
                submission,
                storage,
            } => {
                let (_, mut failure) = client_failure(submission);
                failure.code = "settlement_warning_unsaved".into();
                failure.settlement_bucket = Some(*bucket);
                failure.settlement_warning = Some(SettlementWarningOutcome::of(storage));
                (NeedsUser, failure)
            }
            _ => failure(UNCLASSIFIED.0, UNCLASSIFIED.1),
        };
        into_error(categorized)
    }
}

/// What became of a settlement bucket's warning when the wallet couldn't save or check it
/// (`settlement_warning_unsaved`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SettlementWarningOutcome {
    /// The warning wasn't saved, or its status couldn't be read: nothing of it was applied.
    Failed,
    /// The store can't tell whether the warning was saved: it may already be.
    OutcomeUnknown,
}

impl SettlementWarningOutcome {
    /// The outcome of the warning's store write that failed with `storage`.
    fn of(storage: &PaykitError) -> Self {
        match storage {
            PaykitError::Client(error)
                if matches!(error.unkept(), AppError::Store(StoreError::Unknown)) =>
            {
                Self::OutcomeUnknown
            }
            _ => Self::Failed,
        }
    }
}

/// The exact payment refusal and its confirmation evidence retained by the SDK.
#[derive(uniffi::Object)]
pub struct PaymentRefusal(pub(crate) Refusal);

impl PaymentRefusal {
    /// The case and code it comes as. An attempt that failed comes as its cause's case: it was
    /// closed with nothing paid.
    fn case(&self) -> (Category, &'static str) {
        let (category, code) = refusal_category(&self.kind());
        match &self.0 {
            Refusal::AttemptFailed(cause) => (PaymentRefusal((**cause).clone()).case().0, code),
            _ => (category, code),
        }
    }
}

/// Only the code: the SDK's refusal text can quote what the switch or the network said.
impl std::fmt::Debug for PaymentRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PaymentRefusal")
            .field(&self.case().1)
            .finish()
    }
}

#[uniffi::export]
impl PaymentRefusal {
    /// A fixed description for display: the refusal's code and what its case means. The SDK's
    /// own refusal text can quote what the switch or the network said, so it never leaves
    /// the bindings; the refusal's data is in `kind()`.
    pub fn reason(&self) -> String {
        let (category, code) = self.case();
        format!("{code}: {}", category.meaning())
    }
    /// For a failed earlier attempt, return the underlying reason when available.
    pub fn attempted_failure(&self) -> Option<Arc<Self>> {
        match &self.0 {
            Refusal::AttemptFailed(r) => Some(Arc::new(Self(*r.clone()))),
            _ => None,
        }
    }
}
