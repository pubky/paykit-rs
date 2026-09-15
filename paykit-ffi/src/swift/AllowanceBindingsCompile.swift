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

// This fixture compiles the entire accounting surface; it never executes payments.
private func compileAllowanceAccountingBindingsSurface(
    sdk: PaykitSdkProtocol,
    counterparty: String,
    receiverPath: String,
    allowanceId: String,
    recoveredHistory: AllowanceAccountingHistory,
    preparedAttemptId: String
) async throws {
    let time = "2026-09-15T12:00:00Z"
    let amount = try AccountingAmount(value: "1.000000000000000001", asset: "usd")
    let amountProtocol: AccountingAmountProtocol = amount
    let scope = PaymentRequestScope(
        counterparty: counterparty,
        counterpartyReceiverPath: receiverPath,
        paymentRequestId: "550e8400-e29b-41d4-a716-446655440000"
    )
    let occurrence = PaymentOccurrence(request: scope, billingPeriod: nil)
    let recurringOccurrence = PaymentOccurrence(
        request: scope,
        billingPeriod: BillingPeriod(startsAt: time, endsAt: "2026-09-16T12:00:00Z")
    )
    let checks = PaymentExecutionChecks(
        trustedTime: time,
        paymentEndpointIdentifier: "btc-lightning-bolt11",
        actualAmount: amount,
        endpointCurrent: true,
        localEnabled: true,
        recurrenceEligible: true
    )
    let state: AllowanceAccountingState? = try await sdk.allowanceAccountingState()
    let reconciliation = AllowanceAccountingReconciliation(
        expectedRevision: state?.revision,
        history: recoveredHistory,
        outcomes: [PaymentOutcomeReport(attemptId: preparedAttemptId, outcome: .unknown)],
        trustedTime: time
    )
    let recovered: AllowanceAccountingState = try await sdk.reconcileAllowanceAccounting(
        reconciliation: reconciliation
    )
    let candidates: [AllowanceCandidate] = try await sdk.evaluateAllowanceCandidates(
        scope: scope, trustedTime: time
    )
    let selection = AllowanceSelectionInput(
        allowanceId: allowanceId, expectedRevision: nil, trustedTime: time
    )
    let association: AllowanceAssociationRecord = try await sdk.selectAllowance(
        scope: scope, selection: selection
    )
    let accepted = try await sdk.acceptPaymentRequestAutomatically(
        scope: scope, selection: selection, checks: checks
    )
    let deferred = try await sdk.deferPaymentOccurrence(occurrence: occurrence, reason: "retry later")
    let manualOnly = try await sdk.markPaymentManualOnly(occurrence: occurrence)
    let reassociation = AllowanceReassociationInput(
        allowanceId: allowanceId,
        expectedRevision: 1,
        effectiveFrom: "2026-09-16T12:00:00Z",
        authorizationId: "550e8400-e29b-41d4-a716-446655440000",
        trustedTime: time
    )
    let replacement = try await sdk.authorizeAllowanceReassociation(
        scope: scope, reassociation: reassociation
    )
    let reserved: PaymentAttemptDecision = try await sdk.reserveAutomaticPayment(
        occurrence: recurringOccurrence, expectedAssociationRevision: 1, checks: checks
    )
    let manual = try await sdk.reserveManualPayment(occurrence: occurrence, checks: checks)
    let handoff = try await sdk.beginPaymentExecution(attemptId: preparedAttemptId, checks: checks)
    let reported: PaymentAttemptRecord = try await sdk.recordPaymentOutcome(
        report: PaymentOutcomeReport(attemptId: preparedAttemptId, outcome: .succeeded)
    )
    let failed: PaymentOutcome = .failed
    let phases: [PaymentExecutionStatus] = [.prepared, .submitted, .unknown, .succeeded, .failed]
    let modes: [PaymentExecutionMode] = [.automatic, .manual]
    let decisions: [PaymentDisposition] = [.automatic, .manualOnly, .deferred(reason: "private reason")]
    let blocked = PaymentAttemptDecision.blocked(reason: .sharedRule(code: "clock_rollback"))
    let ready = PaymentAttemptDecision.ready(attempt: reported)
    _ = (amountProtocol, amount.value(), amount.asset(), amount.description, amount.debugDescription,
         recovered, candidates, association, accepted, deferred, manualOnly, replacement, reserved,
         manual, handoff, reported, failed, phases, modes, decisions, blocked, ready)
}
