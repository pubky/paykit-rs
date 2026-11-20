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

private enum CheckoutStep: Equatable, Sendable {
    case feedReady, methodEntered, paymentCancelled
}

private enum CheckoutFailure: Error {
    case timedOut, unexpectedWake(FeedWake), paymentReturned, missingStep
}

private final class CheckoutMethod: ReceiptReader, @unchecked Sendable {
    private let ledger: URL
    private let guardLock = NSLock()
    private var dispatches = 0
    private var pendingLookup = false
    func holdLookup() { guardLock.withLock { pendingLookup = true } }
    func chargedAttempt() throws -> String { try guardLock.withLock { try stored()! } }
    let progress = AsyncThrowingStream<CheckoutStep, Error>.makeStream()
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
        progress.continuation.yield(.methodEntered)
        await release.wait()
        return paid(payment.attemptId)
    }

    override func payment(attemptId: String) async throws -> MethodOutcome? {
        try guardLock.withLock {
            guard try stored() == attemptId else { return nil }
            return pendingLookup ? .pending : paid(attemptId)
        }
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

private func restoreCheckout(
    fixture: CheckoutFixture, source: WalletHost, app: Data, reference: Data,
    method: CheckoutMethod, action: String
) async throws {
    precondition(["confirm", "refuse", "doubt"].contains(action))
    let options = PaymentOptions(endpoint: nil, listApp: nil, publicEndpointsConfirmed: false, unconfirmedIdentityConfirmed: false, confirmedRefusals: [])
    let wallet = try await source.completeEnrollment(reference: reference)
    try await wallet.finishReceive()
    try await wallet.allowConnect(payee: fixture.payee())
    let original = try await fixture.sendCheckout()
    try await wallet.finishReceive()
    let backup = try await wallet.backup()
    try await fixture.passRotationInterval()
    try await wallet.finishReceive()
    if action == "doubt" {
        try await payWhileWaiting(payer: wallet, fixture: fixture, method: method, request: original, options: options)
        method.holdLookup()
    }
    try await wallet.shutdown()

    let store = EnrollmentStore()
    let host = fixture.hostForStore(store: store)
    do {
        _ = try await host.restore(appId: app, bytes: backup, sourceDevice: .unaccountedFor)
        preconditionFailure("an unaccounted source device was restored")
    } catch WalletError.SourceDeviceUnaccountedFor {}
    precondition(store.untouched)
    let restored = try await host.restore(appId: app, bytes: backup, sourceDevice: .inPossession)
    let restoredId = try await restored.appId()
    let initial = try await restored.restoreState()
    precondition(restoredId == app && initial.reconciling)
    var asked = false
    for _ in 0..<8 where !asked {
        let report = try await restored.receive()
        asked = report.events.contains {
            if case let .restoreQuestion(numbers) = $0.data { return numbers == [1] }
            return false
        }
    }
    let shown = try await restored.restoreState()
    precondition(asked && shown.registrations == [1])
    try await restored.shutdown()

    let reopen = try await host.load(appId: app)
    let state = try await reopen.restoreState()
    precondition(state.reconciling && state.cursor != nil && state.registrations == [1])
    do {
        try await reopen.confirmRestoredRegistrations(registrations: [])
        preconditionFailure("the stale restore question was accepted")
    } catch WalletError.Refused {}
    let stillPending = try await reopen.restoreState()
    precondition(stillPending.registrations == state.registrations)
    if action == "refuse" {
        do {
            try await reopen.refuseRestoredRegistrations(registrations: state.registrations)
            preconditionFailure("the unrecognized registration did not stop the wallet")
        } catch WalletError.Operation {}
        let stopped = try await reopen.restoreState()
        guard case let .compromise(n, _)? = stopped.alarm else { preconditionFailure("missing compromise alarm") }
        precondition(n == 1 && method.calls == 0)
    } else {
        try await reopen.confirmRestoredRegistrations(registrations: state.registrations)
        try await reopen.finishReceive()
        let report = try await reopen.reconcile()
        let answered = try await reopen.restoreState()
        precondition(!report.payerSkipped && answered.registrations.isEmpty)
        if action == "doubt" {
            let charged = try method.chargedAttempt()
            let doubtful = report.doubtful.filter { $0.attempts.contains(charged) }
            precondition(doubtful.count == 1)
            try await reopen.resolveConfirmedAttempt(obligation: doubtful[0].obligation, attemptId: charged, resolution: .paid)
            let reviewed = try await reopen.reconcile()
            precondition(reviewed.doubtful.isEmpty && method.calls == 1)
        } else {
            precondition(report.doubtful.isEmpty && method.calls == 0)
        }
        try await reopen.endConfirmedReconciliation()
        let finished = try await reopen.restoreState()
        precondition(!finished.reconciling)
    }
    try await reopen.shutdown()
    if action == "confirm" {
        let payer = try await host.load(appId: app)
        let finished = try await payer.restoreState()
        precondition(!finished.reconciling)
        try await payer.finishReceive()
        let requests = try await payer.requests().filter { $0.id.requestHash == original.requestHash }
        precondition(requests.count == 1)
        let refused = try await payer.payRequest(id: original, options: options)
        guard case let .error(.Refused(confirmation)) = refused.decision,
              case let .heldAfterRestore(record) = confirmation.kind() else {
            preconditionFailure("missing original request hold")
        }
        precondition(record == requests[0].record && method.calls == 0)
        await method.release.signal()
        var confirmed = options
        confirmed.confirmedRefusals = [confirmation]
        let decision = try await payer.payRequest(id: original, options: confirmed)
        guard case let .outcome(.paid(attempt, _)) = decision.decision else { preconditionFailure("restored wallet did not pay the confirmed request") }
        let signed = try await fixture.confirmPayment(request: original)
        try await payer.finishReceive()
        let receipts = try await payer.requestReceipts(id: original)
        precondition(receipts.count == 1 && receipts[0].messageId == signed && receipts[0].attemptId == attempt && method.calls == 1)
        try await payer.shutdown()
    }
}

private func payWhileWaiting(
    payer: NativeWallet, fixture: CheckoutFixture, method: CheckoutMethod,
    request: RequestId, options: PaymentOptions
) async throws {
    let report = method.progress.continuation
    let timer = DispatchSource.makeTimerSource(queue: .global())
    timer.setEventHandler { report.finish(throwing: CheckoutFailure.timedOut) }
    timer.schedule(deadline: .now() + .seconds(15))
    timer.activate()
    defer {
        timer.cancel()
        report.finish()
    }

    let watch = Task {
        do {
            let wake = try await payer.feedWait(seconds: 25)
            guard wake == .ready else { throw CheckoutFailure.unexpectedWake(wake) }
            report.yield(.feedReady)
        } catch { report.finish(throwing: error) }
    }
    let paying = Task {
        do {
            try await fixture.waitForHeldFeed()
            try Task.checkCancellation()
            _ = try await payer.payRequest(id: request, options: options)
            report.finish(throwing: CheckoutFailure.paymentReturned)
        } catch is CancellationError {
            report.yield(.paymentCancelled)
        } catch { report.finish(throwing: error) }
    }

    do {
        var steps = method.progress.stream.makeAsyncIterator()
        let first = try await steps.next()
        let second = try await steps.next()
        switch (first, second) {
        case (.feedReady?, .methodEntered?), (.methodEntered?, .feedReady?): break
        default: throw CheckoutFailure.missingStep
        }
        paying.cancel()
        guard try await steps.next() == .paymentCancelled else { throw CheckoutFailure.missingStep }
    } catch {
        watch.cancel()
        paying.cancel()
        await method.release.signal()
        throw error
    }
    await method.release.signal()
}

func driveCheckout() async throws {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    let method = CheckoutMethod(ledger: directory.appendingPathComponent("settled"))
    let store = EnrollmentStore()
    let fixture = try await enrolledTestCheckout(store: store, method: method)
    precondition(fixture.allowanceRecoveryNoticeKinds() == [.allowanceAcceptanceBeforeLoss, .allowanceWithdrawalConfirmed])
    let reference = Data(repeating: 23, count: 16)
    let secret = Data(repeating: 9, count: 32)
    let owner = WalletEnrollment(store: store)
    let enrolled = try await owner.beginEnrollment(reference: reference)
    let original = try await owner.prepareEnrollment(reference: reference, clientId: "wallet.example", clientKey: Data(repeating: 8, count: 32), capabilities: enrolled.appRecordPath + ":rw", authSecret: secret)
    let approval = try await fixture.approve(request: original.signedRequest, secret: secret)
    try await owner.acceptApproval(reference: reference, identity: fixture.identity(), claim: approval)
    let host = fixture.host()
    if let action = ProcessInfo.processInfo.environment["PAYKIT_TEST_RESTORE"] {
        try await restoreCheckout(fixture: fixture, source: host, app: enrolled.appId, reference: reference, method: method, action: action)
        try await fixture.finish()
        return
    }
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
    try await payer.finishReceive()
    try await payWhileWaiting(payer: payer, fixture: fixture, method: method, request: request, options: options)
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
