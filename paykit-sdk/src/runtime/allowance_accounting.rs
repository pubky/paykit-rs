use super::*;
use crate::domain::allowance_accounting::{self as accounting, *};

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Read retained wallet accounting, including evidence requiring reconciliation.
    pub async fn allowance_accounting_state(&self) -> Result<Option<AllowanceAccountingState>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                Ok(tx.allowance_accounting_state())
            })
            .await
    }

    /// Reconcile complete external wallet history and explicit outcome attestations.
    ///
    /// The wallet must inspect every manual and automatic payment path and its
    /// external idempotency records. An empty input never erases retained history.
    /// No Payment Proof, timeout, or missing local file establishes nonpayment.
    pub async fn reconcile_allowance_accounting(
        &self,
        input: AllowanceAccountingReconciliation,
    ) -> Result<AllowanceAccountingState> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local = self.config.receiver_path.clone();
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                accounting::reconcile(tx, &local, input)
            })
            .await
    }

    /// Evaluate shared static rules and time for each candidate without selecting it.
    /// Blocked evaluation still persists the trusted-time watermark.
    pub async fn evaluate_allowance_candidates(
        &self,
        scope: PaymentRequestScope,
        trusted_time: DateTime<Utc>,
    ) -> Result<Vec<AllowanceCandidate>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local = self.config.receiver_path.clone();
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                accounting::candidates(tx, &local, scope, trusted_time)
            })
            .await
    }

    /// Persist the wallet's explicit Allowance choice without reserving capacity.
    /// Existing choices require their revision; replacement requires explicit reassociation.
    pub async fn select_allowance(
        &self,
        scope: PaymentRequestScope,
        input: AllowanceSelectionInput,
    ) -> Result<AllowanceAssociationRecord> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local = self.config.receiver_path.clone();
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                accounting::select(tx, &local, scope, input, None)
            })
            .await?
            .map_err(selection_block)
    }

    /// Atomically persist selection and queue ordinary Payment Request Acceptance.
    ///
    /// Recurring Acceptance consumes no amount or count. Every actual occurrence
    /// still requires reservation and a fresh handoff check before execution.
    pub async fn accept_payment_request_automatically(
        &self,
        scope: PaymentRequestScope,
        input: AllowanceSelectionInput,
        checks: PaymentExecutionChecks,
    ) -> Result<AllowanceAssociationRecord> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        self.ensure_private_outbound_ready(&scope.counterparty, &scope.counterparty_receiver_path)
            .await?;
        let local = self.config.receiver_path.clone();
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                accounting::select(tx, &local, scope, input, Some(checks))
            })
            .await?
            .map_err(selection_block)
    }

    /// Retain a temporary wallet failure eligible for policy-controlled reconsideration.
    /// This never releases an existing attempt or clears an explicit manual-only decision.
    pub async fn defer_payment_occurrence(
        &self,
        occurrence: PaymentOccurrence,
        reason: String,
    ) -> Result<PaymentOccurrenceRecord> {
        self.set_payment_disposition(occurrence, PaymentDisposition::Deferred { reason })
            .await
    }

    /// Persist an explicit sticky manual-only decision for this occurrence.
    /// No background matcher or reassociation can clear it.
    pub async fn mark_payment_manual_only(
        &self,
        occurrence: PaymentOccurrence,
    ) -> Result<PaymentOccurrenceRecord> {
        self.set_payment_disposition(occurrence, PaymentDisposition::ManualOnly)
            .await
    }

    async fn set_payment_disposition(
        &self,
        occurrence: PaymentOccurrence,
        disposition: PaymentDisposition,
    ) -> Result<PaymentOccurrenceRecord> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local = self.config.receiver_path.clone();
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                accounting::set_disposition(tx, &local, occurrence, disposition)
            })
            .await
    }

    /// Apply explicit user authorization to future recurring Billing Periods only.
    ///
    /// The caller must obtain fresh user approval identifying the replacement
    /// Allowance and future boundary. History, unresolved attempts, and old usage
    /// stay attributed to their original authority. Background matching is not approval.
    pub async fn authorize_allowance_reassociation(
        &self,
        scope: PaymentRequestScope,
        input: AllowanceReassociationInput,
    ) -> Result<AllowanceAssociationRecord> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local = self.config.receiver_path.clone();
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                accounting::reassociate(tx, &local, scope, input)
            })
            .await?
            .map_err(selection_block)
    }

    /// Atomically reserve complete Allowance usage and the shared manual/automatic key.
    /// Ready returns Prepared; it is not yet a wallet handoff permit.
    pub async fn reserve_automatic_payment(
        &self,
        occurrence: PaymentOccurrence,
        expected_association_revision: u64,
        checks: PaymentExecutionChecks,
    ) -> Result<PaymentAttemptDecision> {
        self.reserve_payment(
            occurrence,
            Some(expected_association_revision),
            checks,
            PaymentExecutionMode::Automatic,
        )
        .await
    }

    /// Reserve a manually authorized payment using the same semantic exclusion key.
    /// Manual payments consume no Allowance capacity. The caller supplies user authority.
    pub async fn reserve_manual_payment(
        &self,
        occurrence: PaymentOccurrence,
        checks: PaymentExecutionChecks,
    ) -> Result<PaymentAttemptDecision> {
        self.reserve_payment(occurrence, None, checks, PaymentExecutionMode::Manual)
            .await
    }

    async fn reserve_payment(
        &self,
        occurrence: PaymentOccurrence,
        revision: Option<u64>,
        checks: PaymentExecutionChecks,
        mode: PaymentExecutionMode,
    ) -> Result<PaymentAttemptDecision> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local = self.config.receiver_path.clone();
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                accounting::reserve(tx, &local, occurrence, revision, checks, mode)
            })
            .await
    }

    /// Recheck current lifecycle and selection, then durably issue one wallet handoff.
    ///
    /// Only a Ready Submitted response authorizes immediate execution. Use its
    /// attempt ID for external wallet idempotency. Storage and external settlement
    /// cannot commit atomically: a crash after this call requires reconciliation,
    /// never timeout release or a second execution. Use one coordinated runtime.
    pub async fn begin_payment_execution(
        &self,
        attempt_id: String,
        checks: PaymentExecutionChecks,
    ) -> Result<PaymentAttemptDecision> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local = self.config.receiver_path.clone();
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                accounting::begin(tx, &local, attempt_id, checks)
            })
            .await
    }

    /// Record wallet-attested settlement or definitive failure for one attempt.
    /// A timeout or lost callback is Unknown and continues consuming capacity.
    /// A verified terminal outcome is immutable; Payment Proofs provide no evidence here.
    pub async fn record_payment_outcome(
        &self,
        input: PaymentOutcomeReport,
    ) -> Result<PaymentAttemptRecord> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        self.storage
            .transaction(move |tx| {
                ensure_accounting_identity(tx, &identity)?;
                accounting::report_outcome(tx, input)
            })
            .await
    }
}

fn selection_block(reason: AllowanceAccountingBlock) -> PaykitSdkError {
    PaykitSdkError::Policy {
        context: format!("Allowance selection blocked: {reason:?}"),
        source: None,
    }
}

pub(super) fn ensure_accounting_identity(
    tx: &dyn StorageTransaction,
    expected: &IdentityState,
) -> Result<()> {
    ensure_sign_out_generation(
        tx,
        expected.sign_out_generation,
        "update Allowance accounting",
    )?;
    if tx.load_identity_state().as_ref() != Some(expected) {
        return Err(PaykitSdkError::Policy {
            context: "Payment accounting identity changed during operation".into(),
            source: None,
        });
    }
    Ok(())
}
