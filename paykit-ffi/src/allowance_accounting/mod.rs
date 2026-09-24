//! Thin mobile adapter for the SDK's durable Allowance accounting use cases.
//!
//! This module performs structural conversion only. The SDK owns authority,
//! matching, exact math, lifecycle checks, transactions, and recovery. The wallet
//! remains responsible for trusted time, endpoint usability and payment execution.
mod conversions;
#[cfg(test)]
mod tests;
mod types;
use crate::{FfiPaykitSdk, PaykitFfiError};
pub use types::*;

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Return durable accounting, or None before complete wallet reconciliation.
    pub async fn allowance_accounting_state(
        &self,
    ) -> Result<Option<FfiAllowanceAccountingState>, PaykitFfiError> {
        self.runtime
            .allowance_accounting_state()
            .await?
            .map(TryInto::try_into)
            .transpose()
    }

    /// Merge complete wallet-attested history. An empty history cannot reset existing evidence.
    pub async fn reconcile_allowance_accounting(
        &self,
        reconciliation: FfiAllowanceAccountingReconciliation,
    ) -> Result<FfiAllowanceAccountingState, PaykitFfiError> {
        self.runtime
            .reconcile_allowance_accounting(reconciliation.try_into()?)
            .await?
            .try_into()
    }

    /// Evaluate candidates using shared SDK rules without selecting or reserving an Allowance.
    pub async fn evaluate_allowance_candidates(
        &self,
        scope: FfiPaymentRequestScope,
        trusted_time: String,
    ) -> Result<Vec<FfiAllowanceCandidate>, PaykitFfiError> {
        self.runtime
            .evaluate_allowance_candidates(
                scope.try_into()?,
                conversions::parse_time(trusted_time)?,
            )
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    /// Persist the wallet-selected candidate under the expected association revision.
    pub async fn select_allowance(
        &self,
        scope: FfiPaymentRequestScope,
        selection: FfiAllowanceSelectionInput,
    ) -> Result<FfiAllowanceAssociationRecord, PaykitFfiError> {
        self.runtime
            .select_allowance(scope.try_into()?, selection.try_into()?)
            .await?
            .try_into()
    }

    /// Select and queue automatic Acceptance after SDK validation. This does not reserve or execute a payment.
    pub async fn accept_payment_request_automatically(
        &self,
        scope: FfiPaymentRequestScope,
        selection: FfiAllowanceSelectionInput,
        checks: FfiPaymentExecutionChecks,
    ) -> Result<FfiAllowanceAssociationRecord, PaykitFfiError> {
        self.runtime
            .accept_payment_request_automatically(
                scope.try_into()?,
                selection.try_into()?,
                checks.try_into()?,
            )
            .await?
            .try_into()
    }

    /// Persist temporary deferral; later attempts repeat all SDK and wallet checks.
    pub async fn defer_payment_occurrence(
        &self,
        occurrence: FfiPaymentOccurrence,
        reason: String,
    ) -> Result<FfiPaymentOccurrenceRecord, PaykitFfiError> {
        self.runtime
            .defer_payment_occurrence(occurrence.try_into()?, reason)
            .await?
            .try_into()
    }

    /// Persist a sticky manual-only decision that background candidate matching cannot clear.
    pub async fn mark_payment_manual_only(
        &self,
        occurrence: FfiPaymentOccurrence,
    ) -> Result<FfiPaymentOccurrenceRecord, PaykitFfiError> {
        self.runtime
            .mark_payment_manual_only(occurrence.try_into()?)
            .await?
            .try_into()
    }

    /// Authorize a replacement for future recurring occurrences, preserving previous attempts and usage.
    pub async fn authorize_allowance_reassociation(
        &self,
        scope: FfiPaymentRequestScope,
        reassociation: FfiAllowanceReassociationInput,
    ) -> Result<FfiAllowanceAssociationRecord, PaykitFfiError> {
        self.runtime
            .authorize_allowance_reassociation(scope.try_into()?, reassociation.try_into()?)
            .await?
            .try_into()
    }

    /// Atomically reserve one occurrence. Ready with Prepared status is not an execution permit.
    /// A Blocked value is a successful durable decision, including its updated watermark.
    pub async fn reserve_automatic_payment(
        &self,
        occurrence: FfiPaymentOccurrence,
        expected_association_revision: u64,
        checks: FfiPaymentExecutionChecks,
    ) -> Result<FfiPaymentAttemptDecision, PaykitFfiError> {
        self.runtime
            .reserve_automatic_payment(
                occurrence.try_into()?,
                expected_association_revision,
                checks.try_into()?,
            )
            .await?
            .try_into()
    }

    /// Reserve manual execution under the same semantic dedupe key without consuming Allowance capacity.
    /// A Blocked value is a successful durable decision, including its updated watermark.
    pub async fn reserve_manual_payment(
        &self,
        occurrence: FfiPaymentOccurrence,
        checks: FfiPaymentExecutionChecks,
    ) -> Result<FfiPaymentAttemptDecision, PaykitFfiError> {
        self.runtime
            .reserve_manual_payment(occurrence.try_into()?, checks.try_into()?)
            .await?
            .try_into()
    }

    /// Recheck a preparation and durably issue a handoff before wallet execution. Use the returned attempt ID for executor idempotency.
    /// A Blocked value is a successful durable decision, including its updated watermark.
    pub async fn begin_payment_execution(
        &self,
        attempt_id: String,
        checks: FfiPaymentExecutionChecks,
    ) -> Result<FfiPaymentAttemptDecision, PaykitFfiError> {
        self.runtime
            .begin_payment_execution(attempt_id, checks.try_into()?)
            .await?
            .try_into()
    }

    /// Record wallet-verified settlement. A timeout is Unknown; only definitive failure releases capacity.
    pub async fn record_payment_outcome(
        &self,
        report: FfiPaymentOutcomeReport,
    ) -> Result<FfiPaymentAttemptRecord, PaykitFfiError> {
        self.runtime
            .record_payment_outcome(report.try_into()?)
            .await?
            .try_into()
    }
}
