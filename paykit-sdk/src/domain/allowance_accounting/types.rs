use chrono::{DateTime, Utc};
use paykit_lib::{
    AllowanceId, BillingPeriod, PaymentAmount, PaymentEndpointIdentifier, PaymentRequestId,
};
use serde::{Deserialize, Serialize};

use crate::{AmountRecord, PaykitReceiverPath, PubkyPublicKey};

/// Exact remote Payment Request scope; the SDK supplies the current local identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequestScope {
    /// Authenticated counterparty.
    pub counterparty: PubkyPublicKey,
    /// Counterparty runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Immutable Payment Request identifier.
    pub payment_request_id: PaymentRequestId,
}

/// One payment occurrence supplied by the wallet's scheduler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentOccurrence {
    /// Exact request scope.
    pub request: PaymentRequestScope,
    /// Absent for one-time requests; validated, normalized interval for recurring requests.
    pub billing_period: Option<BillingPeriod>,
}

/// Authoritative identity of a request in durable accounting.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentAccountingScope {
    /// Payer identity, taken from the SDK's identity state.
    pub local_public_key: PubkyPublicKey,
    /// Payer runtime folder, taken from SDK configuration.
    pub local_receiver_path: PaykitReceiverPath,
    /// Authenticated payee identity.
    pub counterparty: PubkyPublicKey,
    /// Authenticated payee runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Stable Payment Request ID.
    pub payment_request_id: String,
}

/// Canonical recurring occurrence interval, independent of wire timestamp spelling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountingBillingPeriod {
    /// Inclusive beginning.
    pub starts_at: DateTime<Utc>,
    /// Exclusive end.
    pub ends_at: DateTime<Utc>,
}

/// Payment dedupe identity shared by manual and automatic execution.
///
/// Allowance ID deliberately does not participate in this key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentOccurrenceKey {
    /// Exact payer, payee, and request scope.
    pub request: PaymentAccountingScope,
    /// Canonical recurring interval, or no interval for a one-time occurrence.
    pub billing_period: Option<AccountingBillingPeriod>,
}

/// Durable wallet decision, separate from payment execution status.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentDisposition {
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentExecutionMode {
    /// Consumes the selected Allowance.
    Automatic,
    /// Does not consume Allowance capacity.
    Manual,
}

/// Durable execution phase. Only definitive failure releases capacity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentExecutionStatus {
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

/// One durable attempt, retaining its original authority and admission time.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentAttemptRecord {
    /// Stable wallet idempotency key, not a Paykit Event ID.
    pub attempt_id: String,
    /// Automatic or manual execution.
    pub mode: PaymentExecutionMode,
    /// Original Allowance; absent for manual execution.
    pub allowance_id: Option<String>,
    /// Association revision used at admission.
    pub association_revision: Option<u64>,
    /// Exact requested amount; excludes fees and refunds.
    pub amount: AmountRecord,
    /// Original trusted admission time.
    pub admitted_at: DateTime<Utc>,
    /// Execution phase.
    pub status: PaymentExecutionStatus,
    /// Recovery epoch binding a prepared token to its original state.
    pub epoch: String,
}

/// Local action state and complete attempts for one semantic payment key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentOccurrenceRecord {
    /// Stable identity across Allowance changes.
    pub key: PaymentOccurrenceKey,
    /// Explicit wallet handling disposition.
    pub disposition: PaymentDisposition,
    /// Persisted selection for this occurrence.
    pub allowance_id: Option<String>,
    /// Selection revision for this occurrence.
    pub association_revision: Option<u64>,
    /// Retained failed, unresolved, and successful attempts.
    pub attempts: Vec<PaymentAttemptRecord>,
}

/// One explicit request-to-Allowance selection decision.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceAssociationRevision {
    /// Monotonic request association revision.
    pub revision: u64,
    /// Selected authority, never combined with another Allowance.
    pub allowance_id: String,
    /// Initial selection has no boundary; replacements apply from this instant.
    pub effective_from: Option<DateTime<Utc>>,
    /// Explicit wallet authorization reference for replacement.
    pub authorization_id: Option<String>,
    /// Trusted decision time.
    pub authorized_at: DateTime<Utc>,
}

/// Complete selection history for one request.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceAssociationRecord {
    /// Exact scoped request.
    pub request: PaymentAccountingScope,
    /// Append-only authorized selections.
    pub revisions: Vec<AllowanceAssociationRevision>,
}

/// Nondecreasing evaluation time for one exact Allowance scope.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceWatermarkRecord {
    /// Local payer identity.
    pub local_public_key: PubkyPublicKey,
    /// Local runtime folder.
    pub local_receiver_path: PaykitReceiverPath,
    /// Remote Allowee identity.
    pub counterparty: PubkyPublicKey,
    /// Remote runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Allowance whose usage and evaluation time are tracked.
    pub allowance_id: String,
    /// Latest trusted evaluation instant, including blocked evaluations.
    pub evaluated_at: DateTime<Utc>,
}

/// Complete wallet-attested accounting history; proofs cannot reconstruct it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceAccountingHistory {
    /// Request selection history.
    pub associations: Vec<AllowanceAssociationRecord>,
    /// Complete semantic payment history through all wallet execution paths.
    pub occurrences: Vec<PaymentOccurrenceRecord>,
    /// Durable trusted time for every evaluated Allowance.
    pub watermarks: Vec<AllowanceWatermarkRecord>,
}

/// Durable ledger coordinated by one SDK runtime and its wallet executor.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceAccountingState {
    /// Monotonic local ledger revision for reconciliation compare-and-set.
    pub revision: u64,
    /// Epoch invalidating prepared handoffs after restore or private-state loss.
    pub epoch: String,
    /// Automatic and manual admission remain blocked until complete reconciliation.
    pub requires_reconciliation: bool,
    /// Complete retained evidence.
    pub history: AllowanceAccountingHistory,
}

/// Initial explicit selection; retries must name the existing revision.
#[derive(Clone, Debug)]
pub struct AllowanceSelectionInput {
    /// Chosen candidate after wallet priority or user choice.
    pub allowance_id: AllowanceId,
    /// Absent only for an initial decision.
    pub expected_revision: Option<u64>,
    /// Wallet-supplied trusted UTC time.
    pub trusted_time: DateTime<Utc>,
}

/// Explicit user-approved replacement for future recurring occurrences.
#[derive(Clone)]
pub struct AllowanceReassociationInput {
    /// Replacement authority on the same link.
    pub allowance_id: AllowanceId,
    /// Expected current association revision.
    pub expected_revision: u64,
    /// Future Billing Period boundary approved by the user.
    pub effective_from: DateTime<Utc>,
    /// Stable UUID-v4 reference to the wallet's explicit user authorization.
    pub authorization_id: String,
    /// Wallet-supplied trusted time; the boundary cannot precede it.
    pub trusted_time: DateTime<Utc>,
}

/// Fresh wallet checks the SDK cannot establish from protocol evidence.
#[derive(Clone, PartialEq, Eq)]
pub struct PaymentExecutionChecks {
    /// Trusted UTC time for this decision.
    pub trusted_time: DateTime<Utc>,
    /// Method actually selected by the wallet.
    pub payment_endpoint_identifier: PaymentEndpointIdentifier,
    /// Amount and asset the wallet has verified will actually be transferred.
    pub actual_amount: PaymentAmount,
    /// Endpoint details are current, usable, and unconsumed.
    pub endpoint_current: bool,
    /// Local enablement and every private safeguard passed.
    pub local_enabled: bool,
    /// Wallet scheduler verified this recurring interval; true for one-time payments.
    pub recurrence_eligible: bool,
}

/// Fixed, redaction-safe reasons why an operation did not proceed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllowanceAccountingBlock {
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

/// Per-Allowance candidate result; this never selects or reserves authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceCandidate {
    /// Candidate Allowance ID.
    pub allowance_id: String,
    /// Static endpoint intersection when eligible.
    pub eligible_payment_endpoint_identifiers: Vec<String>,
    /// Reason it cannot currently be selected.
    pub blocked: Option<AllowanceAccountingBlock>,
}

/// A blocked decision is successful storage work so its watermark remains durable.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentAttemptDecision {
    /// Prepared reservation or freshly issued handoff, according to status.
    Ready {
        /// Authoritative durable attempt; use its stable ID for wallet idempotency.
        attempt: PaymentAttemptRecord,
    },
    /// No new attempt or handoff was authorized.
    Blocked {
        /// Redaction-safe explanation.
        reason: AllowanceAccountingBlock,
    },
}

/// Wallet-verified outcome. A timeout is Unknown, never Failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentOutcome {
    /// Settlement was verified.
    Succeeded,
    /// Terminal failure before settlement was verified.
    Failed,
    /// Settlement remains uncertain.
    Unknown,
}

/// Explicit executor attestation for one stable attempt.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentOutcomeReport {
    /// Attempt returned by admission.
    pub attempt_id: String,
    /// Wallet-verified outcome.
    pub outcome: PaymentOutcome,
}

/// Explicit complete-history reconciliation after initialization, restore, or loss.
///
/// The caller must reconcile external idempotency records and all payment paths.
/// An empty history attests that there were no previous payments; it never
/// clears evidence already retained by this SDK.
#[derive(Clone)]
pub struct AllowanceAccountingReconciliation {
    /// Expected ledger revision, or None only when no ledger exists.
    pub expected_revision: Option<u64>,
    /// Complete recovered history to merge with existing evidence.
    pub history: AllowanceAccountingHistory,
    /// Explicit outcome attestations; unmentioned uncertain attempts stay reserved.
    pub outcomes: Vec<PaymentOutcomeReport>,
    /// Trusted reconciliation time, not inferred from proof receipt times.
    pub trusted_time: DateTime<Utc>,
}

macro_rules! redacted_debug {
    ($($type:ty),+ $(,)?) => {$ (
        impl std::fmt::Debug for $type {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($type), "(<redacted>)"))
            }
        }
    )+ };
}
redacted_debug!(
    PaymentDisposition,
    AllowanceReassociationInput,
    PaymentOutcomeReport,
    PaymentAccountingScope,
    PaymentOccurrenceKey,
    PaymentAttemptRecord,
    PaymentOccurrenceRecord,
    AllowanceAssociationRevision,
    AllowanceAssociationRecord,
    AllowanceWatermarkRecord,
    AllowanceAccountingHistory,
    AllowanceAccountingState,
    PaymentExecutionChecks,
    PaymentAttemptDecision,
    AllowanceAccountingReconciliation
);
