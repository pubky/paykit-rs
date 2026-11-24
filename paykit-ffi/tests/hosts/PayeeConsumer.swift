import Foundation
import Paykit

/// Read the payee's records and check refusals on the enrolled wallet (Paykit spec 6, 8.1, 9).
func checkPayee(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    // Payee reads wait for the wallet's first full read (Paykit spec 6).
    try await wallet.finishReceive()
    let peer = fixture.payee()
    let draft = RequestDraft(
        amount: Amount(asset: "btc", value: "7"),
        destination: .endpoints(endpoints: [ListEndpoint(identifier: "btc-lightning-bolt11", payload: "lnbc1checkout")]),
        paymentReference: "checkout-7", metadataJson: Data(#"{"tax":1.25}"#.utf8)
    )
    func notSent(_ call: () async throws -> Void) async throws {
        do {
            try await call()
            preconditionFailure("invalid message was sent")
        } catch let WalletError.NotSent(failure) {
            precondition(failure.code() == "invalid_input")
        }
    }
    var inconsistent = draft
    inconsistent.acceptedEndpointIdentifiers = ["btc-onchain"]
    try await notSent { _ = try await wallet.sendRequest(to: peer, draft: inconsistent) }
    try await notSent { _ = try await wallet.requestResend(counterparty: peer, selection: ResendSelection()) }

    func missing(_ call: () async throws -> Void) async throws {
        do {
            try await call()
            preconditionFailure("found a proof for an absent record")
        } catch let WalletError.Permanent(failure) {
            precondition(failure.code() == "not_found")
        }
    }
    let absent = Data(repeating: 0, count: 32)
    try await missing { _ = try await wallet.verifyPayment(record: absent) }
    try await missing { _ = try await wallet.confirmPayment(record: absent, details: ReceiptDetails(storeOnly: true)) }
    let activity = try await wallet.payeeActivity()
    let retained = try await wallet.retainedPayeeReceipts()
    precondition(!activity.contains { $0.record == absent } && !retained.contains { $0.record == absent })

    // Sending and cancelling leave signed history. Give those calls their own checkout (Paykit spec 8.1).
    let store = EnrollmentStore()
    // The shop pays through this method, which settles at once and verifies its own proofs.
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    let method = CheckoutMethod(ledger: directory.appendingPathComponent("settled"))
    await method.release.signal()
    let checkout = try await enrolledTestCheckout(store: store, method: method,
                                                 declaration: testDeclaration, endpoints: testEndpoints)
    method.useClock { checkout.nowMs() }
    let reference = Data(repeating: 31, count: 16)
    let secret = Data(repeating: 9, count: 32)
    let owner = WalletEnrollment(store: store)
    let info = try await owner.beginEnrollment(reference: reference)
    let scopes = WalletScopes(pay: true, request: true, contacts: true, manageContacts: true,
                              extensions: ["paykit-cap"])
    let original = try await owner.prepareEnrollment(
        reference: reference, clientId: "wallet.example", clientKey: Data(repeating: 8, count: 32),
        capabilities: info.appRecordPath + ":rw", authSecret: secret, scopes: scopes
    )
    let approval = try await checkout.approve(request: original.signedRequest, secret: secret)
    try await owner.acceptApproval(reference: reference, identity: checkout.identity(), claim: approval)
    let host = checkout.host()
    let seller = try await host.completeEnrollment(reference: reference)
    do {
        _ = try await seller.payeeActivity()
        preconditionFailure("read activity before the first listing")
    } catch let WalletError.NeedsUser(failure) {
        precondition(failure.code() == "not_now")
    }
    try await seller.finishReceive()
    try await checkSentRequest(fixture: checkout, wallet: seller, draft: draft)
    try await seller.shutdown()

    // An unreadable retained receipt fails the entire list (Paykit spec 6).
    let app = URL(fileURLWithPath: info.appRecordPath).deletingPathExtension().lastPathComponent
    let key = Data("p/paykit/rc/unreadable".utf8).base64EncodedString()
    let value = Data("{}".utf8).base64EncodedString()
    store.reopen(namespace: "client/\(app)", entries: ["\(key) \(value)"])
    let reopened = try await host.load(appId: info.appId)
    do {
        _ = try await reopened.retainedPayeeReceipts()
        preconditionFailure("returned a partial receipt list")
    } catch let WalletError.Storage(failure) {
        guard case .Corrupt = failure else { throw failure }
    }
    try await reopened.shutdown()
    try await checkout.finish()
}

private func checkSentRequest(fixture: CheckoutFixture, wallet: NativeWallet, draft: RequestDraft) async throws {
    let peer = fixture.payee()
    let sent = try await wallet.sendRequest(to: peer, draft: draft)
    guard let id = sent.request, id.payee == fixture.identity(), id.requestHash.count == 32,
          UUID(uuidString: sent.msgId) != nil else { preconditionFailure("missing signed request") }
    let delivery = try await wallet.messageDelivery(msgId: sent.msgId)
    precondition(delivery == sent.delivery)
    let activity = try await wallet.payeeActivity()
    guard let row = activity.first(where: { $0.requestHash == id.requestHash }) else {
        preconditionFailure("missing payee activity")
    }
    precondition(row.record.count == 32 && row.payer == peer && row.amount == draft.amount)
    precondition(row.status == .requested && row.attemptId == nil && row.label == nil)
    precondition(!row.divergent && !row.recurring && row.periods.isEmpty)

    let cancellation = try await wallet.cancelSentRequest(record: row.record, reason: "withdrawn")
    precondition(UUID(uuidString: cancellation) != nil && cancellation != sent.msgId)
    let cancelled = try await wallet.payeeActivity()
    precondition(cancelled.contains { $0.record == row.record && $0.status == .cancelled })
    do {
        _ = try await wallet.cancelSentRequest(record: row.record, reason: nil)
        preconditionFailure("cancelled a request twice")
    } catch let WalletError.Permanent(failure) {
        precondition(failure.code() == "cancelled")
    }
    let selection = ResendSelection(acceptedFromMs: fixture.nowMs(), acceptedUntilMs: fixture.nowMs(),
                                    requestHashes: [id.requestHash])
    let resend = try await wallet.requestResend(counterparty: peer, selection: selection)
    precondition(UUID(uuidString: resend) != nil && resend != cancellation)
    let resent = try await wallet.messageDelivery(msgId: resend)
    guard case .accepted? = resent else { preconditionFailure("resend request was not delivered") }

    // The shop pays a second request; the wallet verifies the proof and issues its receipt (Paykit spec 8.1).
    try await wallet.allowConnect(payee: peer)
    guard let paid = try await wallet.sendRequest(to: peer, draft: draft).request else {
        preconditionFailure("missing second signed request")
    }
    try await fixture.payRequest(request: paid)
    try await wallet.finishReceive()
    guard let proven = try await wallet.payeeActivity().first(where: { $0.requestHash == paid.requestHash }) else {
        preconditionFailure("missing paid request")
    }
    precondition(proven.status == .proofReceived && proven.attemptId != nil)
    let verified = try await wallet.verifyPayment(record: proven.record)
    precondition(verified != nil)
    guard let receipt = try await wallet.confirmPayment(record: proven.record, details: nil) else {
        preconditionFailure("missing receipt")
    }
    precondition(UUID(uuidString: receipt) != nil)
    let repeated = try await wallet.confirmPayment(record: proven.record, details: nil)
    precondition(repeated == receipt)
    let receipted = try await wallet.payeeActivity()
    precondition(receipted.contains { $0.record == proven.record && $0.status == .receipted })
}
