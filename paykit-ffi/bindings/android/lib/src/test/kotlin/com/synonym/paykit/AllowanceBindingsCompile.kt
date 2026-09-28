package com.synonym.paykit

// Compile-only consumer fixture for the generated Allowance API.
@Suppress("UNUSED_VARIABLE")
internal suspend fun compileAllowanceBindingsSurface(
    sdk: PaykitSdkInterface,
    counterparty: String,
    paymentAppId: String,
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
        localRole = AllowanceLocalRole.ALLOWER,
        states = listOf(AllowanceLifecycleState.PROPOSED),
    )

    val listed: List<AllowanceRecord> = sdk.listAllowances(filter)
    val found: AllowanceRecord? = sdk.getAllowance(counterparty, allowanceId)
    val proposed: AllowanceRecord = sdk.proposeAllowance(
        counterparty,
        AllowanceLocalRole.ALLOWEE,
        terms,
    )
    val accepted: AllowanceRecord = sdk.acceptAllowance(counterparty, allowanceId)
    val rejected: AllowanceRecord = sdk.rejectAllowance(counterparty, allowanceId)
    val ended: AllowanceRecord = sdk.endAllowance(counterparty, allowanceId)
    val proof = PrivateJsonObject(text = "{}")
    val automaticProof = PaymentProofSubmission(
        billingPeriod = null,
        paymentAppId = paymentAppId,
        paymentEndpointIdentifier = "btc-lightning-bolt11",
        allowanceId = allowanceId,
        conversionQuoteId = null,
        proof = proof,
    )
    val manualProof = PaymentProofSubmission(
        billingPeriod = null,
        paymentAppId = paymentAppId,
        paymentEndpointIdentifier = "btc-lightning-bolt11",
        allowanceId = null,
        conversionQuoteId = null,
        proof = proof,
    )
    val requestWithProof = sdk.submitPaymentProof(
        counterparty,
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
