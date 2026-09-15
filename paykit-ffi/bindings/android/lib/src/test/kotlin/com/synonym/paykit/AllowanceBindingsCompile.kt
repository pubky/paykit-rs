package com.synonym.paykit

// Compile-only consumer fixture for the generated Allowance API.
@Suppress("UNUSED_VARIABLE")
internal suspend fun compileAllowanceBindingsSurface(
    sdk: PaykitSdkInterface,
    counterparty: String,
    receiverPath: String,
    allowanceId: String,
): Pair<AllowanceTermsInterface, AllowanceHistoryStatus> {
    val amountRange = AllowanceAmountRange(minimum = "1", maximum = "10")
    val period = AllowancePeriod(kind = "rolling", every = 1UL, unit = "day", anchor = null)
    val periodLimit = AllowancePeriodLimit(
        amountLimit = "25",
        paymentCountLimit = 2UL,
        period = period,
    )
    val terms = AllowanceTerms(
        asset = "USD",
        perPaymentAmount = amountRange,
        periodLimits = listOf(periodLimit),
        lifetimeAmountLimit = null,
        activeFrom = null,
        expiresAt = null,
        allowedPaymentEndpointIdentifiers = null,
    )
    val filter = AllowanceFilter(
        counterparty = counterparty,
        counterpartyReceiverPath = receiverPath,
        localRole = AllowanceLocalRole.ALLOWER,
        states = listOf(AllowanceLifecycleState.PROPOSED),
    )

    val listed: List<AllowanceRecord> = sdk.listAllowances(filter)
    val found: AllowanceRecord? = sdk.getAllowance(counterparty, receiverPath, allowanceId)
    val proposed: AllowanceRecord = sdk.proposeAllowance(
        counterparty,
        receiverPath,
        AllowanceLocalRole.ALLOWEE,
        terms,
    )
    val accepted: AllowanceRecord = sdk.acceptAllowance(counterparty, receiverPath, allowanceId)
    val rejected: AllowanceRecord = sdk.rejectAllowance(counterparty, receiverPath, allowanceId)
    val ended: AllowanceRecord = sdk.endAllowance(counterparty, receiverPath, allowanceId)
    val proof = PrivateJsonObject(text = "{}")
    val automaticProof = PaymentProofSubmission(
        billingPeriod = null,
        paymentEndpointIdentifier = "btc-lightning-bolt11",
        allowanceId = allowanceId,
        proof = proof,
    )
    val manualProof = PaymentProofSubmission(
        billingPeriod = null,
        paymentEndpointIdentifier = "btc-lightning-bolt11",
        allowanceId = null,
        proof = proof,
    )
    val requestWithProof = sdk.submitPaymentProof(
        counterparty,
        receiverPath,
        "550e8400-e29b-41d4-a716-446655440000",
        automaticProof,
    )
    val reportedAllowances: List<String?> = requestWithProof.paymentProofs.map { it.allowanceId }
    val amountRangeInterface: AllowanceAmountRangeInterface = amountRange
    val periodInterface: AllowancePeriodInterface = period
    val periodLimitInterface: AllowancePeriodLimitInterface = periodLimit
    val privateGetters = listOf(
        amountRange.minimum(),
        period.kind(),
        periodLimit.amountLimit(),
        terms.asset(),
    )

    return terms to AllowanceHistoryStatus.CONSISTENT
}

// Compile-only accounting consumer: no wallet execution occurs here.
@Suppress("UNUSED_VARIABLE")
internal suspend fun compileAllowanceAccountingBindingsSurface(
    sdk: PaykitSdkInterface,
    counterparty: String,
    receiverPath: String,
    allowanceId: String,
    recoveredHistory: AllowanceAccountingHistory,
    preparedAttemptId: String,
) {
    val time = "2026-09-15T12:00:00Z"
    val amount = AccountingAmount(value = "1.000000000000000001", asset = "usd")
    val amountInterface: AccountingAmountInterface = amount
    val scope = PaymentRequestScope(counterparty, receiverPath, "550e8400-e29b-41d4-a716-446655440000")
    val occurrence = PaymentOccurrence(scope, null)
    val recurringOccurrence = PaymentOccurrence(scope, BillingPeriod(time, "2026-09-16T12:00:00Z"))
    val checks = PaymentExecutionChecks(time, "btc-lightning-bolt11", amount, true, true, true)
    val state: AllowanceAccountingState? = sdk.allowanceAccountingState()
    val reconciliation = AllowanceAccountingReconciliation(
        state?.revision, recoveredHistory,
        listOf(PaymentOutcomeReport(preparedAttemptId, PaymentOutcome.UNKNOWN)), time,
    )
    val recovered = sdk.reconcileAllowanceAccounting(reconciliation)
    val candidates: List<AllowanceCandidate> = sdk.evaluateAllowanceCandidates(scope, time)
    val selection = AllowanceSelectionInput(allowanceId, null, time)
    val association: AllowanceAssociationRecord = sdk.selectAllowance(scope, selection)
    val accepted = sdk.acceptPaymentRequestAutomatically(scope, selection, checks)
    val deferred = sdk.deferPaymentOccurrence(occurrence, "retry later")
    val manualOnly = sdk.markPaymentManualOnly(occurrence)
    val reassociation = AllowanceReassociationInput(
        allowanceId, 1UL, "2026-09-16T12:00:00Z",
        "550e8400-e29b-41d4-a716-446655440000", time,
    )
    val replacement = sdk.authorizeAllowanceReassociation(scope, reassociation)
    val reserved: PaymentAttemptDecision = sdk.reserveAutomaticPayment(recurringOccurrence, 1UL, checks)
    val manual = sdk.reserveManualPayment(occurrence, checks)
    val handoff = sdk.beginPaymentExecution(preparedAttemptId, checks)
    val reported: PaymentAttemptRecord = sdk.recordPaymentOutcome(
        PaymentOutcomeReport(preparedAttemptId, PaymentOutcome.SUCCEEDED),
    )
    val failed = PaymentOutcome.FAILED
    val phases = listOf(PaymentExecutionStatus.PREPARED, PaymentExecutionStatus.SUBMITTED,
        PaymentExecutionStatus.UNKNOWN, PaymentExecutionStatus.SUCCEEDED, PaymentExecutionStatus.FAILED)
    val modes = listOf(PaymentExecutionMode.AUTOMATIC, PaymentExecutionMode.MANUAL)
    val decisions = listOf(PaymentDisposition.Automatic, PaymentDisposition.ManualOnly,
        PaymentDisposition.Deferred("private reason"))
    val blocked = PaymentAttemptDecision.Blocked(AllowanceAccountingBlock.SharedRule("clock_rollback"))
    val ready = PaymentAttemptDecision.Ready(reported)
    val privateValues = listOf(amount.value(), amount.asset())
}
