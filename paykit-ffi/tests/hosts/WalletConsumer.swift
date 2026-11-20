import Foundation
import PaykitV2

/// The signed enrollment request can be recovered by a different host handle.
final class EnrollmentStore: WalletStore, @unchecked Sendable {
    private let lock = NSLock()
    private let gate = NSCondition()
    private var holdWrite = false
    private var entered = false
    private var released = false

    func holdNextWrite() {
        gate.lock()
        holdWrite = true
        gate.unlock()
    }

    func waitForChange(_ completed: Bool) async -> Bool {
        await Task.detached { self.waitForChangeBlocking(completed) }.value
    }

    private func waitForChangeBlocking(_ completed: Bool) -> Bool {
        gate.lock()
        defer { gate.unlock() }
        let deadline = Date(timeIntervalSinceNow: 10)
        while !(completed ? released : entered) {
            if !gate.wait(until: deadline) { return false }
        }
        return true
    }

    func continueWrite() {
        gate.lock()
        holdWrite = false
        gate.broadcast()
        gate.unlock()
    }

    private let releaseGate = NSCondition()
    private var gatedNamespace: String?
    private var releasing = false

    func holdRelease(_ namespace: String) {
        releaseGate.lock()
        gatedNamespace = namespace
        releasing = false
        releaseGate.unlock()
    }

    func waitForRelease() async -> Bool {
        await Task.detached { self.waitForReleaseBlocking() }.value
    }

    private func waitForReleaseBlocking() -> Bool {
        releaseGate.lock()
        defer { releaseGate.unlock() }
        let deadline = Date(timeIntervalSinceNow: 5)
        while !releasing {
            if !releaseGate.wait(until: deadline) { return false }
        }
        return true
    }

    func finishSiblingRead(_ namespace: String) -> Bool {
        releaseGate.lock()
        defer { releaseGate.unlock() }
        let stillFenced = gatedNamespace == namespace && lock.withLock { owners[namespace] != nil }
        gatedNamespace = nil
        releaseGate.broadcast()
        return stillFenced
    }

    private func waitBeforeRelease(_ namespace: String) {
        releaseGate.lock()
        defer { releaseGate.unlock() }
        guard gatedNamespace == namespace else { return }
        releasing = true
        releaseGate.broadcast()
        let deadline = Date(timeIntervalSinceNow: 5)
        while gatedNamespace == namespace {
            if !releaseGate.wait(until: deadline) {
                gatedNamespace = nil
                break
            }
        }
    }

    private var acquisitions = 0
    var untouched: Bool { lock.withLock { acquisitions == 0 && rows.isEmpty } }
    private var owners: [String: Data] = [:]
    private var rows: [String: [Data: Data]] = [:]

    func reopen(namespace: String, entries: ArraySlice<String>) {
        for entry in entries {
            let fields = entry.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: false)
            rows[namespace, default: [:]][Data(base64Encoded: String(fields[0]))!] = Data(base64Encoded: String(fields[1]))!
        }
    }

    func get(namespace: String, key: Data) throws -> Data? {
        lock.withLock { rows[namespace]?[key] }
    }
    func scan(namespace: String, prefix: Data, after: Data?, limit: UInt32) throws -> [StoreEntry] {
        lock.withLock {
            (rows[namespace] ?? [:])
                .filter { key, _ in key.starts(with: prefix) && (after.map { $0.lexicographicallyPrecedes(key) } ?? true) }
                .sorted { $0.key.lexicographicallyPrecedes($1.key) }
                .prefix(Int(limit)).map { StoreEntry(key: $0.key, value: $0.value) }
        }
    }
    func acquire(namespace: String, owner: Data) throws -> Bool {
        lock.withLock {
            acquisitions += 1
            guard owners[namespace] == nil else { return false }
            owners[namespace] = owner
            return true
        }
    }
    func release(namespace: String, owner: Data) throws {
        waitBeforeRelease(namespace)
        lock.withLock {
            if owners[namespace] == owner { owners[namespace] = nil }
        }
        gate.lock()
        if entered && !holdWrite { released = true; gate.broadcast() }
        gate.unlock()
    }
    func commitIf(namespace: String, owner: Data, writes: [StoreWrite]) throws -> Bool {
        gate.lock()
        while holdWrite {
            entered = true
            gate.broadcast()
            gate.wait()
        }
        gate.unlock()
        return lock.withLock {
            guard owners[namespace] == owner else { return false }
            for write in writes {
                switch write {
                case let .put(key, value): rows[namespace, default: [:]][key] = value
                case let .delete(key): rows[namespace]?[key] = nil
                case let .deletePrefix(prefix):
                    rows[namespace] = rows[namespace]?.filter { !$0.key.starts(with: prefix) }
                }
            }
            return true
        }
    }
}

class ReceiptReader: WalletHomeserver, PaymentMethod, @unchecked Sendable {
    private let lock = NSLock()
    private var transfers = 0
    var transferCount: Int { lock.withLock { transfers } }
    func get(identity: Data, path: String) async throws -> Data? { nil }
    func put(identity: Data, path: String, bytes: Data) async throws { preconditionFailure("unexpected public write") }
    func delete(identity: Data, path: String) async throws { preconditionFailure("unexpected public delete") }
    func declaration() -> MethodDeclaration {
        MethodDeclaration(idempotent: true, idempotencyRetentionMs: nil, history: true, historyRetentionMs: nil, historySurvivesRestore: true, longestCompletionMs: 3_600_000)
    }
    func selectEndpoints(candidates: [EndpointCandidate], amount: Amount?) throws -> SelectedEndpoints {
        SelectedEndpoints(payable: candidates.map(\.id), failures: [])
    }
    func transfer(endpoint: String, payload: String, amount: Amount) -> MethodTransfer {
        try! MethodTransfer(amount: amount, maxFee: nil)
    }
    func fractionDigits(endpoint: String) -> UInt32? { 8 }
    func pay(payment: MethodPayment) async throws -> MethodOutcome {
        lock.withLock { transfers += 1 }
        preconditionFailure("a receipted payment cannot be dispatched again")
    }
    func payment(attemptId: String) async throws -> MethodOutcome? { nil }
    func allowanceHistory(payer: Data) async throws -> [MethodDispatch]? { nil }
    func verify(proof: MethodProof) async throws -> UInt64? { nil }
    func settlementId(proofJson: Data) -> Data? { nil }
}

private func walletRows(_ issuer: String, _ mode: String) throws -> [String] {
    let producer = Process()
    producer.executableURL = URL(fileURLWithPath: issuer)
    producer.arguments = [mode]
    let pipe = Pipe()
    producer.standardOutput = pipe
    try producer.run()
    let output = pipe.fileHandleForReading.readDataToEndOfFile()
    producer.waitUntilExit()
    let values = String(decoding: output, as: UTF8.self).split(separator: "\n").map(String.init)
    precondition(producer.terminationStatus == 0)
    return values
}

private func recoverReceipt(_ issuer: String) async throws {
    let values = try walletRows(issuer, "paid-wallet")
    precondition(values.count > 7)
    func bytes(_ i: Int) -> Data { Data(base64Encoded: values[i])! }
    let app = bytes(0)
    let id = RequestId(payee: bytes(2), requestHash: bytes(3))
    let method = ReceiptReader()
    let original = EnrollmentStore()
    original.reopen(namespace: values[6], entries: values.dropFirst(7))
    let sameDevice = try WalletHost(store: original, homeserver: method, method: method, endpointIdentifiers: ["btc-lightning-bolt11"])
    let reopened = try await sameDevice.load(appId: app)
    let checkedId = try await reopened.appId()
    let checkedIdentity = try await reopened.identity()
    let backup = try await reopened.backup()
    precondition(checkedId == app && checkedIdentity == bytes(1) && !backup.isEmpty)
    let receipts = try await reopened.requestReceipts(id: id)
    let journal = try await reopened.payments()
    precondition(journal.count == 1 && journal[0].receipt?.messageId == receipts.first?.messageId)
    precondition(journal[0].amount.value == "7" && journal[0].attemptId == values[4])
    precondition(receipts.count == 1 && receipts[0].attemptId == values[4] && receipts[0].messageId == values[5])
    precondition(receipts[0].paymentReference == "checkout-7" && method.transferCount == 0)
    let seller = try walletRows(issuer, "seller-wallet")
    precondition(seller.count > 2 && seller[1] != values[6])
    original.reopen(namespace: seller[1], entries: seller.dropFirst(2))
    let sellerId = Data(base64Encoded: seller[0])!
    let second = try await sameDevice.load(appId: sellerId)
    original.holdRelease(values[6])
    let closing = Task { try await reopened.shutdown() }
    let entered = await original.waitForRelease()
    precondition(entered)
    let siblingId = try await second.appId()
    precondition(siblingId == sellerId && original.finishSiblingRead(values[6]),
                 "seller wallet could not progress while payer release was gated")
    try await closing.value

    var dropped: NativeWallet? = try await sameDevice.load(appId: app)
    let reopenedId = try await dropped!.appId()
    precondition(reopenedId == app)
    original.holdRelease(values[6])
    dropped = nil
    let dropping = await original.waitForRelease()
    precondition(dropping)
    let sellerIdentity = try await second.identity()
    precondition(sellerIdentity.count == 32 && original.finishSiblingRead(values[6]),
                 "seller wallet could not progress while the last payer handle was released")
    try await second.shutdown()
}

private func acknowledgeRecovery(_ issuer: String, cleared: Bool) async throws {
    let producer = Process()
    producer.executableURL = URL(fileURLWithPath: issuer)
    producer.arguments = [cleared ? "cleared-wallet" : "notice-wallet"]
    let output = Pipe()
    producer.standardOutput = output
    try producer.run()
    let bytes = output.fileHandleForReading.readDataToEndOfFile()
    producer.waitUntilExit()
    let values = String(decoding: bytes, as: UTF8.self).split(separator: "\n").map(String.init)
    precondition(producer.terminationStatus == 0 && values.count > 6)
    let app = Data(base64Encoded: values[0])!
    let generation = UInt64(values[1])!
    let duty = Data(base64Encoded: values[4])!
    let store = EnrollmentStore()
    store.reopen(namespace: values[5], entries: values.dropFirst(6))
    let method = ReceiptReader()
    let host = try WalletHost(store: store, homeserver: method, method: method, endpointIdentifiers: ["btc-lightning-bolt11"])
    let wallet = try await host.load(appId: app)
    let matching = try await wallet.unacknowledgedEvents().filter { $0.id == duty }
    precondition(matching.count == 1 && matching[0].owners == ["paykit"])
    let current = try await wallet.recoveryRequired()
    switch matching[0].data {
    case let .recoveryNoticeCleared(original):
        precondition(cleared && current == nil && original == generation)
    case let .recoveryRequired(notice):
        precondition(!cleared && notice == current && notice.generation == generation)
        precondition(notice.reason == .behind && !notice.permanent)
        precondition(notice.firstSeenMs == UInt64(values[2]) && notice.serviceTimeMs == UInt64(values[3]))
    default:
        preconditionFailure("unexpected recovery duty")
    }
    try await wallet.acknowledgeEvents(through: duty)
    try await wallet.shutdown()
    let reopened = try await host.load(appId: app)
    let pending = try await reopened.unacknowledgedEvents()
    let held = try await reopened.recoveryRequired()
    precondition(!pending.contains { $0.id == duty } && held?.generation == (cleared ? nil : generation))
    precondition(method.transferCount == 0)
    try await reopened.shutdown()
}

@main
private enum WalletConsumer {
    static func main() async throws {
        #if PAYKIT_CHECKOUT_TEST
        try await driveCheckout()
        #endif
        let store = EnrollmentStore()
        let reference = Data(repeating: 7, count: 16)
        let key = Data(repeating: 8, count: 32)
        let secret = Data(repeating: 9, count: 32)
        func host() -> WalletEnrollment {
            WalletEnrollment(store: store)
        }
        let original: EnrollmentRequest
        do {
            let first = host()
            let entry = try await first.beginEnrollment(reference: reference)
            do {
                _ = try await first.prepareEnrollment(reference: reference, clientId: "wallet.example",
                    clientKey: key, capabilities: "/pub/another.json:rw", authSecret: secret)
                preconditionFailure("invalid app capabilities were pinned")
            } catch WalletError.InvalidInput(let field) { precondition(field == "capabilities") }
            original = try await first.prepareEnrollment(reference: reference, clientId: "wallet.example", clientKey: key, capabilities: entry.appRecordPath + ":rw", authSecret: secret)
        }
        let next = host()
        let entry = try await next.beginEnrollment(reference: reference)
        precondition(entry.appId == original.appId)
        let caps = entry.appRecordPath + ":rw"
        let resumed = try await next.prepareEnrollment(reference: reference, clientId: "wallet.example", clientKey: key, capabilities: caps, authSecret: secret)
        precondition(resumed.requestHash == original.requestHash && resumed.signedRequest == original.signedRequest)
        precondition(String(reflecting: resumed) == "EnrollmentRequest(<redacted>)")
        precondition(Mirror(reflecting: resumed).children.isEmpty)
        do {
            _ = try await next.prepareEnrollment(reference: reference, clientId: "wallet.example", clientKey: key, capabilities: caps, authSecret: Data(repeating: 10, count: 32))
            preconditionFailure("changed secret accepted")
        } catch WalletError.EnrollmentConflict {}
        // The Rust future can stop awaiting this blocking callback. Until its actual
        // commit finishes the same physical namespace must remain exclusively owned.
        let interrupted = Data(repeating: 13, count: 16)
        let pending = try await next.beginEnrollment(reference: interrupted)
        let interruptedCaps = pending.appRecordPath + ":rw"
        store.holdNextWrite()
        let task = Task {
            try await next.prepareEnrollment(reference: interrupted, clientId: "wallet.example",
                clientKey: key, capabilities: interruptedCaps, authSecret: secret)
        }
        let arrived = await store.waitForChange(false)
        precondition(arrived)
        task.cancel()
        do {
            _ = try await task.value
            preconditionFailure("the blocked Swift task continued waiting")
        } catch is CancellationError {}
        do {
            _ = try await next.beginEnrollment(reference: interrupted)
            preconditionFailure("the detached commit lost ownership")
        } catch WalletError.Storage(let failure) {
            guard case .InUse = failure else { throw failure }
        }
        store.continueWrite()
        let released = await store.waitForChange(true)
        precondition(released)
        let reopened = host()
        let recovered = try await reopened.prepareEnrollment(reference: interrupted, clientId: "wallet.example",
            clientKey: key, capabilities: interruptedCaps, authSecret: secret)
        precondition(recovered.appId == pending.appId)
        let again = try await reopened.prepareEnrollment(reference: interrupted, clientId: "wallet.example",
            clientKey: key, capabilities: interruptedCaps, authSecret: secret)
        precondition(recovered.signedRequest == again.signedRequest)
        guard let issuer = ProcessInfo.processInfo.environment["PAYKIT_TEST_ISSUER"] else {
            preconditionFailure("the enrolled signer is required")
        }
        let process = Process()
        let input = Pipe()
        let output = Pipe()
        process.executableURL = URL(fileURLWithPath: issuer)
        process.standardInput = input
        process.standardOutput = output
        try process.run()
        try input.fileHandleForWriting.write(contentsOf: original.signedRequest)
        try input.fileHandleForWriting.close()
        let bytes = output.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        let lines = String(decoding: bytes, as: UTF8.self).split(separator: "\n")
        precondition(process.terminationStatus == 0 && lines.count == 2)
        guard let identity = Data(base64Encoded: String(lines[0])),
              let claim = Data(base64Encoded: String(lines[1])) else { preconditionFailure("invalid signer reply") }
        try await next.acceptApproval(reference: reference, identity: identity, claim: claim)
        try await host().acceptApproval(reference: reference, identity: identity, claim: claim)
        do {
            try await host().acceptApproval(reference: reference, identity: identity, claim: Data([1]))
            preconditionFailure("replaced the stored approval")
        } catch WalletError.EnrollmentConflict {}
        try await recoverReceipt(issuer)
        try await acknowledgeRecovery(issuer, cleared: false)
        try await acknowledgeRecovery(issuer, cleared: true)
    }
}
