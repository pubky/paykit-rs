import Foundation
import Paykit

private func allowanceUsageRefusal(_ call: () async throws -> Void) async throws {
    do {
        try await call()
        preconditionFailure("expected an Allowance usage refusal")
    } catch WalletError.NotSent(let failure) {
        precondition(failure.code() == "usage")
    }
}

private func allowanceNotFound(_ call: () async throws -> Void) async throws {
    do {
        try await call()
        preconditionFailure("expected an absent Allowance")
    } catch WalletError.Permanent(let failure) {
        precondition(failure.code() == "not_found")
    }
}

private func allowanceNotNow(_ call: () async throws -> Void) async throws {
    do {
        try await call()
        preconditionFailure("expected an Allowance to be unavailable")
    } catch WalletError.NeedsUser(let failure) {
        precondition(failure.code() == "not_now")
    }
}

/// Propose, inspect and withdraw the wallet's own pending Allowances (Paykit spec 8.3.1).
func checkAllowances(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let peer = fixture.payee()
    let terms = AllowanceTerms(
        asset: "btc", perPaymentAmount: PerPaymentRange(minimum: Amount(asset: "btc", value: "0.01"),
                                                        maximum: Amount(asset: "btc", value: "2.00")),
        periodLimits: [PeriodLimit(amountLimit: nil, paymentCountLimit: 3, period: .rolling(every: 1, unit: "week"))],
        lifetimeAmountLimit: Amount(asset: "btc", value: "10.0"), activeFrom: nil, expiresAt: nil,
        allowedPaymentEndpointIdentifiers: ["btc-lightning-bolt11"]
    )

    // Read the unlisted backup in a separate store, before its first receive (Paykit spec 6).
    let host = fixture.hostForStore(store: EnrollmentStore())
    let restored = try await host.restore(appId: wallet.appId(), bytes: wallet.backup(), sourceDevice: .inPossession)
    do {
        try await allowanceNotNow { _ = try await restored.allowances() }
    } catch {
        try await restored.shutdown()
        throw error
    }
    try await restored.shutdown()
    let before = try await wallet.allowances().map(\.allowanceId)
    do {
        _ = try await wallet.proposeExecutableAllowance(to: peer, terms: terms, userConfirmed: false)
        preconditionFailure("proposed automatic payments without confirmation")
    } catch WalletError.InvalidInput(let field) {
        precondition(field == "user_confirmed")
    }

    var proposals: [SentAllowance] = []
    func withdraw() async throws {
        for proposal in proposals {
            let end = try await wallet.endAllowance(record: proposal.record)
            precondition(end != proposal.msgId && UUID(uuidString: end) != nil)
        }
    }
    do {
        // Eight own pending proposals to one counterparty fill its limit (Paykit spec 8.3.1).
        for index in 0..<8 {
            let role: AllowanceRole = index == 0 ? .allower : .allowee
            let sent: SentAllowance
            if role == .allowee {
                sent = try await wallet.proposeAllowance(to: peer, role: role, terms: terms)
            } else {
                sent = try await wallet.proposeExecutableAllowance(to: peer, terms: terms, userConfirmed: true)
            }
            proposals.append(sent)
            precondition(sent.record.count == 32 && UUID(uuidString: sent.allowanceId) != nil)
            precondition(UUID(uuidString: sent.msgId) != nil)
            let delivery = try await wallet.messageDelivery(msgId: sent.msgId)
            precondition(delivery == sent.delivery)
            let view = try await wallet.allowance(record: sent.record)
            precondition(view.allowanceId == sent.allowanceId && view.record == sent.record)
            precondition(view.counterparty == peer && view.proposedHere && view.role == role)
            precondition(view.status == .proposed && view.terms == terms)
            precondition(!view.consented && !view.confirmationNeeded && view.executorAppId == nil)
            precondition(view.blocked == false && view.hold.record() == sent.record)
        }
        let listed = try await wallet.allowances().map(\.allowanceId)
        precondition(Set(listed) == Set(before + proposals.map(\.allowanceId)))
        try await allowanceNotNow { _ = try await wallet.proposeAllowance(to: peer, role: .allowee, terms: terms) }
        try await allowanceNotNow {
            _ = try await wallet.proposeExecutableAllowance(to: peer, terms: terms, userConfirmed: true)
        }

        guard let record = proposals.first?.record else { preconditionFailure("missing proposal") }
        try await allowanceUsageRefusal { _ = try await wallet.acceptAllowance(record: record) }
        try await allowanceUsageRefusal { _ = try await wallet.acceptExecutableAllowance(record: record, userConfirmed: true) }
        do {
            _ = try await wallet.acceptExecutableAllowance(record: record, userConfirmed: false)
            preconditionFailure("accepted automatic payments without confirmation")
        } catch WalletError.InvalidInput(let field) {
            precondition(field == "user_confirmed")
        }
        try await allowanceUsageRefusal { _ = try await wallet.rejectAllowance(record: record) }
        try await allowanceUsageRefusal { try await wallet.discardPendingAllowance(record: record) }
    } catch {
        try await withdraw()
        throw error
    }
    try await withdraw()
    guard let withdrawn = proposals.first?.record else { preconditionFailure("missing withdrawal") }
    try await allowanceNotFound { _ = try await wallet.endAllowance(record: withdrawn) }
    try await allowanceNotFound { _ = try await wallet.allowance(record: withdrawn) }
    let remaining = try await wallet.allowances().map(\.allowanceId)
    precondition(Set(remaining) == Set(before))
}
