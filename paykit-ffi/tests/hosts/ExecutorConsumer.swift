import Foundation
import Paykit

/// This account has never handed off a payment (Paykit spec 8.3).
private final class EmptyExecutorAccount: ReceiptReader, @unchecked Sendable {
    override func allowanceHistory(payer: Data) async throws -> [MethodHandoff]? { [] }
}

private func checkExecutorCheckout(
    fixture: CheckoutFixture, wallet: NativeWallet, account: EmptyExecutorAccount?
) async throws {
    try await wallet.finishReceive()
    let alarm = try await wallet.ledgerAlarm()
    precondition(alarm == nil)
    let id = try await fixture.proposeAllowance(role: .allower)
    try await wallet.finishReceive()
    guard let offer = try await wallet.allowances().first(where: { $0.allowanceId == id }) else {
        preconditionFailure("missing Allowance proposal")
    }
    precondition(offer.role == .allower && !offer.proposedHere && offer.status == .proposed)
    let record = offer.record
    _ = try await wallet.acceptExecutableAllowance(record: record, userConfirmed: true)
    do {
        try await wallet.resumeAllowanceExecutor(record: record)
        let view = try await wallet.allowance(record: record)
        let appId = try await wallet.appId()
        precondition(view.status == .accepted && view.consented && view.executorAppId == appId)
        do {
            try await wallet.confirmAllowanceExecutor(hold: view.hold, userConfirmed: false)
            preconditionFailure("renewed executor consent without confirmation")
        } catch WalletError.InvalidInput(let field) {
            precondition(field == "user_confirmed")
        }
        try await wallet.confirmAllowanceExecutor(hold: view.hold, userConfirmed: true)
        let request = try await fixture.sendCheckout()
        try await wallet.finishReceive()
        let result = try await wallet.payUnderAllowance(id: request, allowance: record)
        precondition(result.publicFailures.isEmpty)
        if let account {
            guard case let .error(.Permanent(failure)) = result.decision else {
                preconditionFailure("expected the per-payment limit to refuse the checkout")
            }
            precondition(failure.code() == "not_automatic" && account.transferCount == 0)
        } else {
            guard case let .error(.NeedsUser(failure)) = result.decision else {
                preconditionFailure("expected unavailable account history to hold the payment")
            }
            precondition(failure.code() == "allowance_execution_held")
        }
        let entries = try await wallet.payments()
        precondition(!entries.contains { $0.requestHash == request.requestHash })
    } catch {
        _ = try await wallet.endAllowance(record: record)
        throw error
    }
    _ = try await wallet.endAllowance(record: record)
    let ended = try await wallet.allowance(record: record)
    precondition(ended.status == .ended)
}

/// The shared wallet lacks account-wide history. A fresh account can verify its empty ledger
/// and refuse the same checkout at the per-payment limit (Paykit spec 8.3).
func checkExecutor(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    try await checkExecutorCheckout(fixture: fixture, wallet: wallet, account: nil)
    let account = EmptyExecutorAccount()
    let store = EnrollmentStore()
    let checkout = try await enrolledTestCheckout(store: store, method: account,
                                                 declaration: testDeclaration, endpoints: testEndpoints)
    let reference = Data(repeating: 31, count: 16)
    let secret = Data(repeating: 9, count: 32)
    let scopes = WalletScopes(pay: true, allowance: true, allowanceExecute: true, manageContacts: true)
    let owner = WalletEnrollment(store: store)
    let info = try await owner.beginEnrollment(reference: reference)
    let original = try await owner.prepareEnrollment(
        reference: reference, clientId: "wallet.example", clientKey: Data(repeating: 8, count: 32),
        capabilities: info.appRecordPath + ":rw", authSecret: secret, scopes: scopes
    )
    let approval = try await checkout.approve(request: original.signedRequest, secret: secret)
    try await owner.acceptApproval(reference: reference, identity: checkout.identity(), claim: approval)
    let payer = try await checkout.host().completeEnrollment(reference: reference)
    do {
        try await payer.allowConnect(payee: checkout.payee())
        try await checkExecutorCheckout(fixture: checkout, wallet: payer, account: account)
    } catch {
        try await payer.shutdown()
        try await checkout.finish()
        throw error
    }
    try await payer.shutdown()
    try await checkout.finish()
}
