import Foundation
import PaykitV2

/// The signed enrollment request can be recovered by a different host handle.
private final class EnrollmentStore: WalletStore, @unchecked Sendable {
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

    private var owners: [String: Data] = [:]
    private var rows: [String: [Data: Data]] = [:]

    func get(namespace: String, key: Data) throws -> Data? {
        lock.withLock { rows[namespace]?[key] }
    }
    func acquire(namespace: String, owner: Data) throws -> Bool {
        lock.withLock {
            guard owners[namespace] == nil else { return false }
            owners[namespace] = owner
            return true
        }
    }
    func release(namespace: String, owner: Data) throws {
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
            precondition(writes.count == 1)
            let key: Data
            let value: Data
            switch writes[0] { case let .put(k, v): key = k; value = v }
            rows[namespace, default: [:]][key] = value
            return true
        }
    }
}

@main
private enum WalletConsumer {
    static func main() async throws {
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
    }
}
