// Compile-only consumer fixture for the generated Allowance API.
private func compileAllowanceBindingsSurface(
    sdk: PaykitSdkProtocol,
    counterparty: String,
    receiverPath: String,
    allowanceId: String
) async throws {
    let amountRange = try AllowanceAmountRange(minimum: "1", maximum: "10")
    let period = try AllowancePeriod(kind: "rolling", every: 1, unit: "day", anchor: nil)
    let periodLimit = try AllowancePeriodLimit(
        amountLimit: "25",
        paymentCountLimit: 2,
        period: period
    )
    let terms = try AllowanceTerms(
        asset: "USD",
        perPaymentAmount: amountRange,
        periodLimits: [periodLimit],
        lifetimeAmountLimit: nil,
        activeFrom: nil,
        expiresAt: nil,
        allowedPaymentEndpointIdentifiers: nil
    )
    let amountRangeProtocol: AllowanceAmountRangeProtocol = amountRange
    let periodProtocol: AllowancePeriodProtocol = period
    let periodLimitProtocol: AllowancePeriodLimitProtocol = periodLimit
    let termsProtocol: AllowanceTermsProtocol = terms
    let filter = AllowanceFilter(
        counterparty: counterparty,
        counterpartyReceiverPath: receiverPath,
        localRole: .allower,
        states: [.proposed]
    )
    let historyStatus: AllowanceHistoryStatus = .consistent

    let listed: [AllowanceRecord] = try await sdk.listAllowances(filter: filter)
    let found: AllowanceRecord? = try await sdk.getAllowance(
        counterparty: counterparty,
        counterpartyReceiverPath: receiverPath,
        allowanceId: allowanceId
    )
    let proposed: AllowanceRecord = try await sdk.proposeAllowance(
        counterparty: counterparty,
        counterpartyReceiverPath: receiverPath,
        localRole: .allowee,
        terms: terms
    )
    let accepted: AllowanceRecord = try await sdk.acceptAllowance(
        counterparty: counterparty,
        counterpartyReceiverPath: receiverPath,
        allowanceId: allowanceId
    )
    let rejected: AllowanceRecord = try await sdk.rejectAllowance(
        counterparty: counterparty,
        counterpartyReceiverPath: receiverPath,
        allowanceId: allowanceId
    )
    let ended: AllowanceRecord = try await sdk.endAllowance(
        counterparty: counterparty,
        counterpartyReceiverPath: receiverPath,
        allowanceId: allowanceId
    )

    let proof = try PrivateJsonObject(text: "{}")
    let automaticProof = PaymentProofSubmission(
        billingPeriod: nil,
        paymentEndpointIdentifier: "btc-lightning-bolt11",
        allowanceId: allowanceId,
        proof: proof
    )
    let manualProof = PaymentProofSubmission(
        billingPeriod: nil,
        paymentEndpointIdentifier: "btc-lightning-bolt11",
        allowanceId: nil,
        proof: proof
    )
    let requestWithProof = try await sdk.submitPaymentProof(
        counterparty: counterparty,
        counterpartyReceiverPath: receiverPath,
        paymentRequestId: "550e8400-e29b-41d4-a716-446655440000",
        proof: automaticProof
    )
    let reportedAllowances: [String?] = requestWithProof.paymentProofs.map { $0.allowanceId }

    let privateGetters = (
        amountRange.minimum(),
        period.kind(),
        periodLimit.period(),
        terms.perPaymentAmount()
    )
    let redactedDescriptions = [
        amountRange.description,
        amountRange.debugDescription,
        period.description,
        period.debugDescription,
        periodLimit.description,
        periodLimit.debugDescription,
        terms.description,
        terms.debugDescription,
    ]

    _ = (
        amountRangeProtocol,
        periodProtocol,
        periodLimitProtocol,
        termsProtocol,
        privateGetters,
        redactedDescriptions,
        historyStatus,
        listed,
        found,
        proposed,
        accepted,
        rejected,
        ended,
        manualProof,
        reportedAllowances
    )
}
