import Foundation
import PaykitV2

private actor PaymentGate {
    private var open = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func wait() async {
        if !open { await withCheckedContinuation { waiters.append($0) } }
    }
    func signal() {
        open = true
        for waiter in waiters { waiter.resume() }
        waiters.removeAll()
    }
}

private final class CheckoutMethod: ReceiptReader, @unchecked Sendable {
    private let ledger: URL
    private let guardLock = NSLock()
    private var dispatches = 0
    let entered = PaymentGate()
    let release = PaymentGate()
    var calls: Int { guardLock.withLock { dispatches } }

    init(ledger: URL) { self.ledger = ledger }

    private func originalProof(_ attempt: String) -> Data {
        Data("{\"settlement\":\"\(attempt)\"}".utf8)
    }
    private func stored() throws -> String? {
        guard FileManager.default.fileExists(atPath: ledger.path) else { return nil }
        return try String(contentsOf: ledger, encoding: .utf8)
    }
    private func paid(_ attempt: String) -> MethodOutcome {
        .paid(proofJson: originalProof(attempt), paidAtMs: nil)
    }

    override func pay(payment: MethodPayment) async throws -> MethodOutcome {
        try guardLock.withLock {
            dispatches += 1
            precondition(payment.endpoint == "btc-lightning-bolt11" && payment.payload == "lnbc1checkout")
            precondition(payment.amount.asset == "btc" && payment.amount.value == "7")
            precondition(payment.paymentReference == "checkout-7")
            if let previous = try stored() {
                precondition(previous == payment.attemptId)
            } else {
                try Data(payment.attemptId.utf8).write(to: ledger, options: .atomic)
                let output = try FileHandle(forWritingTo: ledger)
                try output.synchronize()
                try output.close()
            }
        }
        await entered.signal()
        await release.wait()
        return paid(payment.attemptId)
    }

    override func payment(attemptId: String) async throws -> MethodOutcome? {
        try guardLock.withLock { try stored() == attemptId ? paid(attemptId) : nil }
    }
    override func verify(proof: MethodProof) async throws -> UInt64? {
        try guardLock.withLock {
            guard try stored() == proof.attemptId, proof.proofJson == originalProof(proof.attemptId),
                  proof.amount.asset == "btc", proof.amount.value == "7", proof.endpoint == "btc-lightning-bolt11",
                  proof.payload == nil || proof.payload == "lnbc1checkout" else { return nil }
            return 1
        }
    }
    override func settlementId(proofJson: Data) -> Data? {
        guardLock.withLock {
            guard let attempt = try? stored(), proofJson == originalProof(attempt) else { return nil }
            return Data(attempt.utf8)
        }
    }
}

private extension NativeWallet {
    func finishReceive() async throws {
        for _ in 0..<8 { if !(try await receive()).more { return } }
        preconditionFailure("wallet did not complete its feed")
    }
}

func driveCheckout() async throws {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    let method = CheckoutMethod(ledger: directory.appendingPathComponent("settled"))
    let store = EnrollmentStore()
    let fixture = try await enrolledTestCheckout(store: store, method: method)
    let reference = Data(repeating: 23, count: 16)
    let secret = Data(repeating: 9, count: 32)
    let owner = WalletEnrollment(store: store)
    let enrolled = try await owner.beginEnrollment(reference: reference)
    let original = try await owner.prepareEnrollment(reference: reference, clientId: "wallet.example", clientKey: Data(repeating: 8, count: 32), capabilities: enrolled.appRecordPath + ":rw", authSecret: secret)
    let approval = try await fixture.approve(request: original.signedRequest, secret: secret)
    try await owner.acceptApproval(reference: reference, identity: fixture.identity(), claim: approval)
    let host = fixture.host()
    let first = try await host.completeEnrollment(reference: reference)
    try await first.finishReceive()
    try await fixture.setAccountMissing(missing: true)
    let stop = try await first.receive()
    let notice = try await first.recoveryRequired()
    guard stop.stopped, let notice, let duty = stop.events.first(where: { event in
        if case let .recoveryRequired(seen) = event.data { return seen == notice }
        return false
    })?.id else { preconditionFailure("missing verified notice") }
    try await first.shutdown()

    let payer = try await host.load(appId: enrolled.appId)
    let pending = try await payer.unacknowledgedEvents()
    precondition(pending.contains { $0.id == duty && $0.owners == ["paykit"] })
    try await payer.acknowledgeEvents(through: duty)
    let held = try await payer.recoveryRequired()
    precondition(held == notice)
    try await fixture.setAccountMissing(missing: false)
    let cleared = try await payer.receive()
    precondition(cleared.events.contains { e in
        if case let .recoveryNoticeCleared(generation) = e.data { return generation == notice.generation }
        return false
    })
    try await payer.allowConnect(payee: fixture.payee())
    let request = try await fixture.sendCheckout()
    try await payer.finishReceive()
    let views = try await payer.requests()
    precondition(views.count == 1 && views[0].id.requestHash == request.requestHash && views[0].id.payee == request.payee)
    precondition(views[0].paymentReference == "checkout-7" && views[0].amount.value == "7")
    let options = PaymentOptions(endpoint: nil, listApp: nil, publicEndpointsConfirmed: false, unconfirmedIdentityConfirmed: false, confirmedRefusals: [])
    let missing = RequestId(payee: request.payee, requestHash: Data(repeating: 0, count: 32))
    let skipped = try await payer.payRequest(id: missing, options: options)
    guard case let .error(.Refused(refusal)) = skipped.decision,
          case .notFound = refusal.kind() else { preconditionFailure("missing request was not refused") }
    precondition(skipped.publicFailures.isEmpty && method.calls == 0)
    let paying = Task { _ = try await payer.payRequest(id: request, options: options) }
    await method.entered.wait()
    paying.cancel()
    do { try await paying.value; preconditionFailure("cancelled native payment returned success") }
    catch is CancellationError {}
    await method.release.signal()
    try await payer.shutdown()

    let reopened = try await host.load(appId: enrolled.appId)
    let entries = try await reopened.payments()
    precondition(entries.count == 1 && entries[0].stage == .paying && entries[0].amount.value == "7")
    let resumed = try await reopened.resumePayments()
    precondition(resumed.contains {
        if case let .paid(id, _) = $0 { return id == entries[0].attemptId }
        return false
    })
    let signed = try await fixture.confirmPayment(request: request)
    try await reopened.finishReceive()
    let receipts = try await reopened.requestReceipts(id: request)
    let final = try await reopened.payments()
    precondition(receipts.count == 1 && receipts[0].messageId == signed && receipts[0].attemptId == entries[0].attemptId)
    precondition(receipts[0].payee == request.payee && receipts[0].requestHash == request.requestHash)
    precondition(receipts[0].paymentReference == "checkout-7" && final[0].receipt?.messageId == signed && method.calls == 1)
    try await reopened.shutdown()
    try await fixture.finish()
}
