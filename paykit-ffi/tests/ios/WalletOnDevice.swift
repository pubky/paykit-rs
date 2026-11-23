import Foundation
import PaykitV2
import XCTest

/// Whether each store call and synchronous method callback ran on the main thread.
private final class CallbackThreads: @unchecked Sendable {
    private let lock = NSLock()
    private var onMain: [Bool] = []
    func record() { lock.withLock { onMain.append(Thread.isMainThread) } }
    var calls: [Bool] { lock.withLock { onMain } }
}

private final class DeviceMethod: CheckoutMethod, @unchecked Sendable {
    let threads = CallbackThreads()
    override func declaration() -> MethodDeclaration {
        threads.record()
        return super.declaration()
    }
    override func selectEndpoints(candidates: [EndpointCandidate], amount: Amount?) throws -> SelectedEndpoints {
        threads.record()
        return try super.selectEndpoints(candidates: candidates, amount: amount)
    }
    override func transfer(endpoint: String, payload: String, amount: Amount) -> MethodTransfer {
        threads.record()
        return super.transfer(endpoint: endpoint, payload: payload, amount: amount)
    }
    override func fractionDigits(endpoint: String) -> UInt32? {
        threads.record()
        return super.fractionDigits(endpoint: endpoint)
    }
    override func settlementId(proofJson: Data) -> Data? {
        threads.record()
        return super.settlementId(proofJson: proofJson)
    }
}

private final class DeviceStore: WalletStore, @unchecked Sendable {
    private let inner = EnrollmentStore()
    let threads: CallbackThreads
    init(threads: CallbackThreads) { self.threads = threads }
    func get(namespace: String, key: Data) throws -> Data? {
        threads.record()
        return try inner.get(namespace: namespace, key: key)
    }
    func scan(namespace: String, prefix: Data, after: Data?, limit: UInt32) throws -> [StoreEntry] {
        threads.record()
        return try inner.scan(namespace: namespace, prefix: prefix, after: after, limit: limit)
    }
    func acquire(namespace: String, owner: Data) throws -> Bool {
        threads.record()
        return try inner.acquire(namespace: namespace, owner: owner)
    }
    func release(namespace: String, owner: Data) throws {
        threads.record()
        try inner.release(namespace: namespace, owner: owner)
    }
    func commitIf(namespace: String, owner: Data, writes: [StoreWrite]) throws -> Bool {
        threads.record()
        return try inner.commitIf(namespace: namespace, owner: owner, writes: writes)
    }
}

/// A full checkout driven from the main actor, on Paykit's 4 MiB threads, which this simulator
/// must accept. It records the thread of every store call and synchronous method callback.
final class WalletOnDeviceTests: XCTestCase {
    @MainActor
    func test_wallet_checkout_from_the_main_actor_runs_off_it() async throws {
        XCTAssertTrue(Thread.isMainThread)
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let method = DeviceMethod(ledger: directory.appendingPathComponent("settled"))
        await method.release.signal()
        let store = DeviceStore(threads: method.threads)
        let fixture = try await enrolledTestCheckout(store: store, method: method)
        method.useClock { fixture.nowMs() }
        let reference = Data(repeating: 23, count: 16)
        let secret = Data(repeating: 9, count: 32)
        let owner = WalletEnrollment(store: store)
        let info = try await owner.beginEnrollment(reference: reference)
        let request = try await owner.prepareEnrollment(reference: reference, clientId: "wallet.example",
            clientKey: Data(repeating: 8, count: 32), capabilities: info.appRecordPath + ":rw", authSecret: secret)
        let approval = try await fixture.approve(request: request.signedRequest, secret: secret)
        try await owner.acceptApproval(reference: reference, identity: fixture.identity(), claim: approval)
        let wallet = try await fixture.host().completeEnrollment(reference: reference)
        try await wallet.finishReceive()
        try await wallet.allowConnect(payee: fixture.payee())
        let sent = try await fixture.sendCheckout()
        try await wallet.finishReceive()
        let options = PaymentOptions(endpoint: nil, listApp: nil, publicEndpointsConfirmed: false,
            unconfirmedIdentityConfirmed: false, confirmedRefusals: [])
        let result = try await wallet.payRequest(id: sent, options: options)
        guard case let .outcome(.paid(attempt, _)) = result.decision else { return XCTFail("the checkout was not paid") }
        let receipt = try await fixture.confirmPayment(request: sent)
        try await wallet.finishReceive()
        let receipts = try await wallet.requestReceipts(id: sent)
        XCTAssertEqual(receipts.map(\.messageId), [receipt])
        XCTAssertEqual(receipts.map(\.attemptId), [attempt])
        XCTAssertEqual(method.calls, 1)
        // Every store call and synchronous method callback ran on Paykit's threads.
        XCTAssertFalse(method.threads.calls.isEmpty)
        XCTAssertFalse(method.threads.calls.contains(true))
        try await wallet.shutdown()
        try await fixture.finish()
    }
}
