//! Typed mobile inputs and durable accounting records.
use crate::{errors::validation_error, FfiBillingPeriod, PaykitFfiError};
use std::{fmt, sync::Arc};

/// Validated private Payment Amount. Default native formatting is redacted.
#[uniffi::export(Debug, Display)]
#[derive(uniffi::Object)]
pub struct FfiAccountingAmount {
    pub(super) amount: paykit_lib::PaymentAmount,
}
impl fmt::Debug for FfiAccountingAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccountingAmount(<redacted>)")
    }
}
impl fmt::Display for FfiAccountingAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}
#[uniffi::export]
impl FfiAccountingAmount {
    /// Validate exact decimal and asset text without rounding or converting precision.
    #[uniffi::constructor]
    pub fn new(value: String, asset: String) -> Result<Self, PaykitFfiError> {
        Ok(Self {
            amount: paykit_lib::PaymentAmount::new(value, asset)
                .map_err(|_| validation_error("Invalid accounting Payment Amount"))?,
        })
    }
    /// Explicitly access the sensitive decimal spelling.
    pub fn value(&self) -> String {
        self.amount.value().to_owned()
    }
    /// Explicitly access the asset spelling.
    pub fn asset(&self) -> String {
        self.amount.asset().to_owned()
    }
}

/// Exact remote Payment Request scope; the SDK supplies the current local identity.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentRequestScope {
    /// Authenticated counterparty.
    pub counterparty: String,
    /// Counterparty runtime folder.
    pub counterparty_receiver_path: String,
    /// Immutable Payment Request identifier.
    pub payment_request_id: String,
}

/// One payment occurrence supplied by the wallet's scheduler.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentOccurrence {
    /// Exact request scope.
    pub request: FfiPaymentRequestScope,
    /// Absent for one-time requests; validated, normalized interval for recurring requests.
    pub billing_period: Option<FfiBillingPeriod>,
}

/// Authoritative identity of a request in durable accounting.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentAccountingScope {
    /// Payer identity, taken from the SDK's identity state.
    pub local_public_key: String,
    /// Payer runtime folder, taken from SDK configuration.
    pub local_receiver_path: String,
    /// Authenticated payee identity.
    pub counterparty: String,
    /// Authenticated payee runtime folder.
    pub counterparty_receiver_path: String,
    /// Stable Payment Request ID.
    pub payment_request_id: String,
}

/// Canonical recurring occurrence interval, independent of wire timestamp spelling.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAccountingBillingPeriod {
    /// Inclusive beginning.
    pub starts_at: String,
    /// Exclusive end.
    pub ends_at: String,
}

/// Payment dedupe identity shared by manual and automatic execution.
///
/// Allowance ID deliberately does not participate in this key.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentOccurrenceKey {
    /// Exact payer, payee, and request scope.
    pub request: FfiPaymentAccountingScope,
    /// Canonical recurring interval, or no interval for a one-time occurrence.
    pub billing_period: Option<FfiAccountingBillingPeriod>,
}

/// One durable attempt, retaining its original authority and admission time.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentAttemptRecord {
    /// Stable wallet idempotency key, not a Paykit Event ID.
    pub attempt_id: String,
    /// Automatic or manual execution.
    pub mode: FfiPaymentExecutionMode,
    /// Original Allowance; absent for manual execution.
    pub allowance_id: Option<String>,
    /// Association revision used at admission.
    pub association_revision: Option<u64>,
    /// Exact requested amount; excludes fees and refunds.
    pub amount: Arc<FfiAccountingAmount>,
    /// Original trusted admission time.
    pub admitted_at: String,
    /// Execution phase.
    pub status: FfiPaymentExecutionStatus,
    /// Recovery epoch binding a prepared token to its original state.
    pub epoch: String,
}

/// Local action state and complete attempts for one semantic payment key.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentOccurrenceRecord {
    /// Stable identity across Allowance changes.
    pub key: FfiPaymentOccurrenceKey,
    /// Explicit wallet handling disposition.
    pub disposition: FfiPaymentDisposition,
    /// Persisted selection for this occurrence.
    pub allowance_id: Option<String>,
    /// Selection revision for this occurrence.
    pub association_revision: Option<u64>,
    /// Retained failed, unresolved, and successful attempts.
    pub attempts: Vec<FfiPaymentAttemptRecord>,
}

/// One explicit request-to-Allowance selection decision.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAllowanceAssociationRevision {
    /// Monotonic request association revision.
    pub revision: u64,
    /// Selected authority, never combined with another Allowance.
    pub allowance_id: String,
    /// Initial selection has no boundary; replacements apply from this instant.
    pub effective_from: Option<String>,
    /// Explicit wallet authorization reference for replacement.
    pub authorization_id: Option<String>,
    /// Trusted decision time.
    pub authorized_at: String,
}

/// Complete selection history for one request.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAllowanceAssociationRecord {
    /// Exact scoped request.
    pub request: FfiPaymentAccountingScope,
    /// Append-only authorized selections.
    pub revisions: Vec<FfiAllowanceAssociationRevision>,
}

/// Nondecreasing evaluation time for one exact Allowance scope.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAllowanceWatermarkRecord {
    /// Local payer identity.
    pub local_public_key: String,
    /// Local runtime folder.
    pub local_receiver_path: String,
    /// Remote Allowee identity.
    pub counterparty: String,
    /// Remote runtime folder.
    pub counterparty_receiver_path: String,
    /// Allowance whose usage and evaluation time are tracked.
    pub allowance_id: String,
    /// Latest trusted evaluation instant, including blocked evaluations.
    pub evaluated_at: String,
}

/// Complete wallet-attested accounting history; proofs cannot reconstruct it.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAllowanceAccountingHistory {
    /// Request selection history.
    pub associations: Vec<FfiAllowanceAssociationRecord>,
    /// Complete semantic payment history through all wallet execution paths.
    pub occurrences: Vec<FfiPaymentOccurrenceRecord>,
    /// Durable trusted time for every evaluated Allowance.
    pub watermarks: Vec<FfiAllowanceWatermarkRecord>,
}

/// Durable ledger coordinated by one SDK runtime and its wallet executor.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAllowanceAccountingState {
    /// Monotonic local ledger revision for reconciliation compare-and-set.
    pub revision: u64,
    /// Epoch invalidating prepared handoffs after restore or private-state loss.
    pub epoch: String,
    /// Automatic and manual admission remain blocked until complete reconciliation.
    pub requires_reconciliation: bool,
    /// Complete retained evidence.
    pub history: FfiAllowanceAccountingHistory,
}

/// Initial explicit selection; retries must name the existing revision.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAllowanceSelectionInput {
    /// Chosen candidate after wallet priority or user choice.
    pub allowance_id: String,
    /// Absent only for an initial decision.
    pub expected_revision: Option<u64>,
    /// Wallet-supplied trusted UTC time.
    pub trusted_time: String,
}

/// Explicit user-approved replacement for future recurring occurrences.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAllowanceReassociationInput {
    /// Replacement authority on the same link.
    pub allowance_id: String,
    /// Expected current association revision.
    pub expected_revision: u64,
    /// Future Billing Period boundary approved by the user.
    pub effective_from: String,
    /// Stable UUID-v4 reference to the wallet's explicit user authorization.
    pub authorization_id: String,
    /// Wallet-supplied trusted time; the boundary cannot precede it.
    pub trusted_time: String,
}

/// Fresh wallet checks the SDK cannot establish from protocol evidence.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentExecutionChecks {
    /// Trusted UTC time for this decision.
    pub trusted_time: String,
    /// Method actually selected by the wallet.
    pub payment_endpoint_identifier: String,
    /// Amount and asset the wallet has verified will actually be transferred.
    pub actual_amount: Arc<FfiAccountingAmount>,
    /// Endpoint details are current, usable, and unconsumed.
    pub endpoint_current: bool,
    /// Local enablement and every private safeguard passed.
    pub local_enabled: bool,
    /// Wallet scheduler verified this recurring interval; true for one-time payments.
    pub recurrence_eligible: bool,
}

/// Per-Allowance candidate result; this never selects or reserves authority.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAllowanceCandidate {
    /// Candidate Allowance ID.
    pub allowance_id: String,
    /// Static endpoint intersection when eligible.
    pub eligible_payment_endpoint_identifiers: Vec<String>,
    /// Reason it cannot currently be selected.
    pub blocked: Option<FfiAllowanceAccountingBlock>,
}

/// Explicit executor attestation for one stable attempt.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentOutcomeReport {
    /// Attempt returned by admission.
    pub attempt_id: String,
    /// Wallet-verified outcome.
    pub outcome: FfiPaymentOutcome,
}

/// Explicit complete-history reconciliation after initialization, restore, or loss.
///
/// The caller must reconcile external idempotency records and all payment paths.
/// An empty history attests that there were no previous payments; it never
/// clears evidence already retained by this SDK.
///
/// Treat these fields as private wallet data; do not log or describe the record.
#[derive(uniffi::Record, Clone)]
pub struct FfiAllowanceAccountingReconciliation {
    /// Expected ledger revision, or None only when no ledger exists.
    pub expected_revision: Option<u64>,
    /// Complete recovered history to merge with existing evidence.
    pub history: FfiAllowanceAccountingHistory,
    /// Explicit outcome attestations; unmentioned uncertain attempts stay reserved.
    pub outcomes: Vec<FfiPaymentOutcomeReport>,
    /// Trusted reconciliation time, not inferred from proof receipt times.
    pub trusted_time: String,
}

/// Durable wallet decision, separate from payment execution status.
#[derive(uniffi::Enum, Clone)]
pub enum FfiPaymentDisposition {
    /// Automatic reconsideration may proceed under the persisted selection.
    Automatic,
    /// Temporary failure; reconsideration must repeat all checks.
    Deferred {
        /// Private wallet reason, bounded to 256 characters.
        reason: String,
    },
    /// Sticky explicit decision; background matching never clears it.
    ManualOnly,
}

/// Source of payment authorization for accounting purposes.
#[derive(uniffi::Enum, Clone)]
pub enum FfiPaymentExecutionMode {
    /// Consumes the selected Allowance.
    Automatic,
    /// Does not consume Allowance capacity.
    Manual,
}

/// Durable execution phase. Only definitive failure releases capacity.
#[derive(uniffi::Enum, Clone)]
pub enum FfiPaymentExecutionStatus {
    /// Capacity held; no handoff permit has been issued.
    Prepared,
    /// Handoff issued; settlement must be reconciled even after a crash.
    Submitted,
    /// Outcome is uncertain, including a stale restored preparation.
    Unknown,
    /// Wallet verified success; committed usage never decreases.
    Succeeded,
    /// Wallet confirmed terminal failure before settlement.
    Failed,
}

/// Fixed, redaction-safe reasons why an operation did not proceed.
#[derive(uniffi::Enum, Clone)]
pub enum FfiAllowanceAccountingBlock {
    /// Accounting is absent or requires wallet reconciliation.
    ReconciliationRequired,
    /// Request or Allowance lifecycle, authenticated role, or history is unsuitable.
    InvalidLifecycle,
    /// The expected association revision is obsolete.
    StaleRevision,
    /// No persisted Allowance selection exists.
    NoSelection,
    /// An explicit manual-only decision excludes automatic handling.
    ManualOnly,
    /// A successful or unresolved attempt already owns this occurrence.
    PaymentAlreadyRecorded,
    /// Endpoint, amount, scheduling, or private wallet checks failed.
    WalletChecksFailed,
    /// Shared exact amount, time, or capacity rules failed.
    SharedRule {
        /// Stable evaluator code, without private amounts.
        code: String,
    },
}

/// A blocked decision is successful storage work so its watermark remains durable.
#[derive(uniffi::Enum, Clone)]
pub enum FfiPaymentAttemptDecision {
    /// Prepared reservation or freshly issued handoff, according to status.
    Ready {
        /// Authoritative durable attempt; use its stable ID for wallet idempotency.
        attempt: FfiPaymentAttemptRecord,
    },
    /// No new attempt or handoff was authorized.
    Blocked {
        /// Redaction-safe explanation.
        reason: FfiAllowanceAccountingBlock,
    },
}

/// Wallet-verified outcome. A timeout is Unknown, never Failed.
#[derive(uniffi::Enum, Clone)]
pub enum FfiPaymentOutcome {
    /// Settlement was verified.
    Succeeded,
    /// Terminal failure before settlement was verified.
    Failed,
    /// Settlement remains uncertain.
    Unknown,
}

macro_rules! redacted_debug {
    ($($type:ty),+ $(,)?) => {$ (
        impl fmt::Debug for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($type), "(<redacted>)"))
            }
        }
    )+ };
}
redacted_debug!(
    FfiPaymentRequestScope,
    FfiPaymentOccurrence,
    FfiPaymentAccountingScope,
    FfiAccountingBillingPeriod,
    FfiPaymentOccurrenceKey,
    FfiPaymentDisposition,
    FfiPaymentExecutionMode,
    FfiPaymentExecutionStatus,
    FfiPaymentAttemptRecord,
    FfiPaymentOccurrenceRecord,
    FfiAllowanceAssociationRevision,
    FfiAllowanceAssociationRecord,
    FfiAllowanceWatermarkRecord,
    FfiAllowanceAccountingHistory,
    FfiAllowanceAccountingState,
    FfiAllowanceSelectionInput,
    FfiAllowanceReassociationInput,
    FfiPaymentExecutionChecks,
    FfiAllowanceAccountingBlock,
    FfiAllowanceCandidate,
    FfiPaymentAttemptDecision,
    FfiPaymentOutcome,
    FfiPaymentOutcomeReport,
    FfiAllowanceAccountingReconciliation
);
