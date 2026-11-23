import Foundation
import Paykit

actor PaymentGate {
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

enum CheckoutStep: Equatable, Sendable {
    case feedReady, methodEntered
}

private enum CheckoutFailure: Error {
    case timedOut, unexpectedWake(FeedWake), missingStep
}

class CheckoutMethod: ReceiptReader, @unchecked Sendable {
    private let ledger: URL
    private let guardLock = NSLock()
    private var dispatches = 0
    private var pendingLookup = false
    /// The test services' clock, which dates each payment as a real method's evidence would.
    private var clock: (() -> UInt64)?
    private var paidAt: UInt64?
    func useClock(_ now: @escaping () -> UInt64) { guardLock.withLock { clock = now } }
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
                paidAt = clock!()
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
            return paidAt
        }
    }
    override func settlementId(proofJson: Data) throws -> Data? {
        guardLock.withLock {
            guard let attempt = try? stored(), proofJson == originalProof(attempt) else { return nil }
            return Data(attempt.utf8)
        }
    }
}

extension NativeWallet {
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
        // The wallet stops waiting at its pay time limit and closes while the host's pay
        // still runs: the attempt stays recorded as possibly sent.
        let open = try await payThenCancel(payer: wallet, method: method, request: original, options: options, release: false)
        if case .outcome(.paid) = open.decision { preconditionFailure("an unanswered payment was recorded paid") }
        method.holdLookup()
        try await wallet.shutdown()
        await method.release.signal()
    } else {
        try await wallet.shutdown()
    }

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
    precondition(String(describing: state) == "WalletRestoreState(<redacted>)")
    precondition(String(reflecting: state) == "WalletRestoreState(<redacted>)" && Mirror(reflecting: state).children.isEmpty)
    do {
        try await reopen.confirmRestoredRegistrations(registrations: [])
        preconditionFailure("the stale restore question was accepted")
    } catch let WalletError.NeedsUser(failure) {
        guard case .notNow? = failure.refusal()?.kind() else { preconditionFailure("unexpected restore refusal") }
    }
    let stillPending = try await reopen.restoreState()
    precondition(stillPending.registrations == state.registrations)
    if action == "refuse" {
        do {
            try await reopen.refuseRestoredRegistrations(registrations: state.registrations)
            preconditionFailure("the unrecognized registration did not stop the wallet")
        } catch WalletError.Recovery {}
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
        guard case let .error(.NeedsUser(failure)) = refused.decision, let confirmation = failure.refusal(),
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

/// Start the payment and cancel its Swift task once the host's method is paying. A cancelled
/// task still waits for the wallet's answer, and the payment itself always goes on. With
/// `release` the method answers at once; without it the wallet stops waiting at its pay time
/// limit and records the payment as possibly sent.
private func payThenCancel(
    payer: NativeWallet, method: CheckoutMethod, request: RequestId, options: PaymentOptions,
    waitingAt fixture: CheckoutFixture? = nil, release: Bool
) async throws -> PaymentResult {
    let report = method.progress.continuation
    let timer = DispatchSource.makeTimerSource(queue: .global())
    timer.setEventHandler { report.finish(throwing: CheckoutFailure.timedOut) }
    timer.schedule(deadline: .now() + .seconds(15))
    timer.activate()
    defer {
        timer.cancel()
        report.finish()
    }

    let watch = fixture.map { _ in
        Task {
            do {
                let wake = try await payer.feedWait(seconds: 25)
                guard wake == .ready else { throw CheckoutFailure.unexpectedWake(wake) }
                report.yield(.feedReady)
            } catch { report.finish(throwing: error) }
        }
    }
    let paying = Task { () throws -> PaymentResult in
        try await fixture?.waitForHeldFeed()
        return try await payer.payRequest(id: request, options: options)
    }

    do {
        var steps = method.progress.stream.makeAsyncIterator()
        let first = try await steps.next()
        if fixture != nil {
            let second = try await steps.next()
            switch (first, second) {
            case (.feedReady?, .methodEntered?), (.methodEntered?, .feedReady?): break
            default: throw CheckoutFailure.missingStep
            }
        } else if first != .methodEntered {
            throw CheckoutFailure.missingStep
        }
        paying.cancel()
    } catch {
        watch?.cancel()
        paying.cancel()
        await method.release.signal()
        throw error
    }
    if release { await method.release.signal() }
    return try await paying.value
}

private extension NativeWallet {
    func followLoss(_ fixture: CheckoutFixture, second: Bool) async throws {
        let before = try await receive().generation
        try await fixture.replaceLostProvider(second: second)
        for _ in 0..<12 {
            let read = try await receive()
            if read.generation > before && !read.stopped && !read.more { return }
        }
        preconditionFailure("wallet did not follow the replacement provider")
    }
    func lossReport() async throws -> WalletReconciliation {
        for _ in 0..<12 {
            do { return try await reconcile() }
            catch let WalletError.NeedsUser(failure) {
                guard case .notNow? = failure.refusal()?.kind() else { throw WalletError.NeedsUser(failure: failure) }
            }
        }
        preconditionFailure("reconciliation did not finish its listing")
    }
}

private func reviewLoss(_ wallet: NativeWallet, _ fixture: CheckoutFixture, _ request: RequestId, _ method: CheckoutMethod) async throws {
    func shown() async throws -> DoubtfulPayment {
        let report = try await wallet.lossReport()
        let rows = report.doubtful.filter { $0.requestHash == request.requestHash }
        guard rows.count == 1, let row = rows.first else { preconditionFailure("missing original pre-loss obligation") }
        precondition(!report.payerSkipped && row.payee == request.payee && row.attempts.isEmpty)
        precondition(row.obligation.count == 32 && row.beforeLoss != nil)
        return row
    }
    let first = try await shown()
    try await wallet.followLoss(fixture, second: true)
    let current = try await shown()
    try await wallet.endConfirmedReconciliation()
    guard let earlier = first.beforeLoss, let latest = current.beforeLoss,
          let oldPayment = earlier.payment, let currentPayment = latest.payment else {
        preconditionFailure("missing original obligation and loss")
    }
    precondition(first.obligation == current.obligation && latest.generation > earlier.generation)
    precondition(latest.uncertainUntilMs >= latest.declaredAtMs)
    do {
        try await wallet.confirmNotPaidBeforeLoss(payment: oldPayment)
        preconditionFailure("the earlier loss was accepted")
    } catch let WalletError.NeedsUser(failure) {
        guard failure.code() == "loss_changed", case let .lossChanged(changed?)? = failure.refusal()?.kind() else {
            throw WalletError.NeedsUser(failure: failure)
        }
        precondition(changed.generation == latest.generation && changed.declaredAtMs == latest.declaredAtMs &&
            changed.uncertainUntilMs == latest.uncertainUntilMs && changed.payment == nil)
    }
    let options = PaymentOptions(endpoint: nil, listApp: nil, publicEndpointsConfirmed: false, unconfirmedIdentityConfirmed: false, confirmedRefusals: [])
    let held = try await wallet.payRequest(id: request, options: options)
    guard case let .error(.NeedsUser(failure)) = held.decision, case let .mayBePaid(doubt?)? = failure.refusal()?.kind() else {
        preconditionFailure("payment should still need its current loss statement")
    }
    precondition(doubt.obligation == first.obligation && doubt.beforeLoss?.generation == latest.generation && method.calls == 0)
    try await wallet.confirmNotPaidBeforeLoss(payment: currentPayment)
    let remaining = try await wallet.reconcile()
    precondition(!remaining.doubtful.contains { $0.obligation == current.obligation })
    try await wallet.allowConnect(payee: fixture.payee())
}

private enum CheckoutReview: CaseIterable { case ordinary, divergence, providerLoss }

/// A method whose synchronous callbacks throw: a declared failure or any other error.
private final class ThrowingMethod: ReceiptReader, @unchecked Sendable {
    struct Crash: Error {}
    let declared: Bool
    init(declared: Bool) { self.declared = declared }
    private func fail() -> Error { declared ? MethodError.Unknown(reason: "callback failed") : Crash() }
    override func selectEndpoints(candidates: [EndpointCandidate], amount: Amount?) throws -> SelectedEndpoints { throw fail() }
    override func transfer(endpoint: String, payload: String, amount: Amount) throws -> MethodTransfer { throw fail() }
    override func settlementId(proofJson: Data) throws -> Data? { throw fail() }
}

/// A method that names the settlement of every proof.
private final class SettlingMethod: ReceiptReader, @unchecked Sendable {
    override func settlementId(proofJson: Data) throws -> Data? { Data("a1".utf8) }
}

/// A callback that throws reads as no answer: no endpoint, an amount to confirm, no settlement.
private func throwingCallbacks() throws {
    let proof = Data(#"{"settlement":"a1"}"#.utf8)
    let answered = try testMethodAnswers(method: SettlingMethod(), declaration: testDeclaration, endpoints: testEndpoints, proofJson: proof)
    precondition(answered.selected == [1] && answered.selectionFailure == nil)
    precondition(answered.transferAmount?.value == "7" && answered.settlementId == Data("a1".utf8))
    for declared in [true, false] {
        let thrown = try testMethodAnswers(method: ThrowingMethod(declared: declared), declaration: testDeclaration, endpoints: testEndpoints, proofJson: proof)
        precondition(thrown.selected == nil && thrown.selectionFailure == "method_unknown")
        precondition(thrown.transferAmount == nil && thrown.settlementId == nil)
    }
}

/// Text the switch gave never leaves the bindings: not in a message, a description or a reason.
private func outsideTextStaysInside() {
    let sentinel = "SENTINEL-outside-text"
    do {
        try testOutsideTextError(text: sentinel)
        preconditionFailure("no error")
    } catch let error as WalletError {
        guard let failure = error.categorized, let refusal = failure.refusal() else { preconditionFailure("no failure") }
        let shown = [
            String(describing: error), String(reflecting: error), error.localizedDescription,
            failure.code(), failure.context(), refusal.reason(),
        ]
        precondition(!shown.contains { $0.contains(sentinel) }, "outside text left the bindings")
        precondition(failure.context() == "switch_unavailable: a later call can succeed")
    } catch {
        preconditionFailure("unexpected error")
    }
}

func driveCheckout() async throws {
    if let action = ProcessInfo.processInfo.environment["PAYKIT_TEST_RESTORE"] {
        try await driveCheckout(.ordinary, restore: action)
    } else {
        outsideTextStaysInside()
        try throwingCallbacks()
        for review in CheckoutReview.allCases { try await driveCheckout(review) }
    }
}

private func driveCheckout(_ recovery: CheckoutReview, restore: String? = nil) async throws {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    let method = CheckoutMethod(ledger: directory.appendingPathComponent("settled"))
    let store = EnrollmentStore()
    let fixture = try await enrolledTestCheckout(store: store, method: method, declaration: testDeclaration, endpoints: testEndpoints)
    method.useClock { fixture.nowMs() }
    precondition(fixture.allowanceRecoveryNoticeKinds() == [.allowanceAcceptanceBeforeLoss, .allowanceWithdrawalConfirmed])
    let reference = Data(repeating: 23, count: 16)
    let secret = Data(repeating: 9, count: 32)
    let owner = WalletEnrollment(store: store)
    let enrolled = try await owner.beginEnrollment(reference: reference)
    let original = try await owner.prepareEnrollment(reference: reference, clientId: "wallet.example", clientKey: Data(repeating: 8, count: 32), capabilities: enrolled.appRecordPath + ":rw", authSecret: secret)
    let approval = try await fixture.approve(request: original.signedRequest, secret: secret)
    try await owner.acceptApproval(reference: reference, identity: fixture.identity(), claim: approval)
    let host = fixture.host()
    if let action = restore {
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

    var payer = try await host.load(appId: enrolled.appId)
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
    if recovery == .providerLoss {
        try await payer.followLoss(fixture, second: false)
        let report = try await payer.lossReport()
        precondition(report.doubtful.isEmpty)
        try await payer.endConfirmedReconciliation()
        try await payer.allowConnect(payee: fixture.payee())
    }
    let request = try await fixture.sendCheckout()
    try await payer.finishReceive()
    let views = try await payer.requests()
    precondition(views.count == 1 && views[0].id.requestHash == request.requestHash && views[0].id.payee == request.payee)
    precondition(views[0].paymentReference == "checkout-7" && views[0].amount.value == "7")
    let options = PaymentOptions(endpoint: nil, listApp: nil, publicEndpointsConfirmed: false, unconfirmedIdentityConfirmed: false, confirmedRefusals: [])
    let missing = RequestId(payee: request.payee, requestHash: Data(repeating: 0, count: 32))
    let skipped = try await payer.payRequest(id: missing, options: options)
    guard case let .error(.Permanent(failure)) = skipped.decision, failure.code() == "not_found",
          case .notFound? = failure.refusal()?.kind() else { preconditionFailure("missing request was not refused") }
    precondition(skipped.publicFailures.isEmpty && method.calls == 0)
    var shown: Divergence?
    switch recovery {
    case .divergence:
        try await payer.shutdown()
        try await fixture.markDivergent(appId: enrolled.appId, record: views[0].record)
        payer = try await host.load(appId: enrolled.appId)
        let held = try await payer.payRequest(id: request, options: options)
        guard case let .error(.NeedsUser(failure)) = held.decision, case let .divergent(records)? = failure.refusal()?.kind(),
              records.count == 1, let observation = records.first else { preconditionFailure("expected a divergent checkout") }
        let listed = try await payer.divergentRecords()
        precondition(listed.count == 1 && listed[0].record == observation.record && observation.record == views[0].record)
        precondition(observation.version != nil && observation.version == listed[0].version && observation.paused &&
            observation.divergent && !observation.mayHoldCredit && method.calls == 0)
        shown = observation
        try await payer.confirmDivergent(observation: observation.observation)
        try await payer.finishReceive()
        let drained = try await payer.divergentRecords()
        precondition(!drained.contains { $0.paused || $0.writesWaiting })
    case .providerLoss:
        try await reviewLoss(payer, fixture, request, method)
    case .ordinary: break
    }
    try await payer.finishReceive()
    let charged = try await payThenCancel(payer: payer, method: method, request: request, options: options, waitingAt: fixture, release: true)
    guard case .outcome(.paid) = charged.decision else { preconditionFailure("the payment was not recorded paid") }
    try await payer.shutdown()

    var reopened = try await host.load(appId: enrolled.appId)
    let entries = try await reopened.payments()
    precondition(entries.count == 1 && [.railPaid, .acceptanceOwed, .finished].contains(entries[0].stage))
    precondition(entries[0].amount.value == "7")
    _ = try await reopened.resumePayments()
    precondition(method.calls == 1)
    if let shown {
        try await reopened.shutdown()
        try await fixture.markDivergent(appId: enrolled.appId, record: shown.record)
        reopened = try await host.load(appId: enrolled.appId)
    }
    let signed = try await fixture.confirmPayment(request: request)
    try await reopened.finishReceive()
    if let shown {
        let listed = try await reopened.divergentRecords()
        guard listed.count == 1, let latest = listed.first else { preconditionFailure("missing current observation") }
        precondition(latest.record == shown.record && latest.version != shown.version)
        do {
            try await reopened.confirmDivergent(observation: shown.observation)
            preconditionFailure("the earlier divergent write was accepted")
        } catch let WalletError.NotSent(failure) {
            guard failure.code() == "conflict" else { preconditionFailure("expected a changed write") }
        }
        let paused = try await reopened.divergentRecords()
        precondition(paused.count == 1 && paused[0].paused)
        try await reopened.confirmDivergent(observation: latest.observation)
        let waiting = try await reopened.divergentRecords()
        precondition(waiting.count == 1 && waiting[0].writesWaiting)
        try await reopened.finishReceive()
        let drained = try await reopened.divergentRecords()
        precondition(!drained.contains { $0.writesWaiting })
    }
    let receipts = try await reopened.requestReceipts(id: request)
    let final = try await reopened.payments()
    precondition(receipts.count == 1 && receipts[0].messageId == signed && receipts[0].attemptId == entries[0].attemptId)
    precondition(receipts[0].payee == request.payee && receipts[0].requestHash == request.requestHash)
    precondition(receipts[0].paymentReference == "checkout-7" && final[0].receipt?.messageId == signed && method.calls == 1)
    try await reopened.shutdown()
    try await fixture.finish()
}
