import Foundation
import PaykitV2
import XCTest

/// Records whether Paykit called the method's synchronous callbacks on the main thread.
private final class DeviceMethod: CheckoutMethod, @unchecked Sendable {
    private let record = NSLock()
    private var onMain: [Bool] = []
    var mainThreadCalls: [Bool] { record.withLock { onMain } }

    override func selectEndpoints(candidates: [EndpointCandidate], amount: Amount?) throws -> SelectedEndpoints {
        record.withLock { onMain.append(Thread.isMainThread) }
        return try super.selectEndpoints(candidates: candidates, amount: amount)
    }
}

/// A full checkout driven from the main actor. Every wallet call and callback runs on
/// Paykit's own threads, whose 4 MiB stacks this simulator must accept.
final class WalletOnDeviceTests: XCTestCase {
    @MainActor
    func test_wallet_checkout_from_the_main_actor_runs_off_it() async throws {
        XCTAssertTrue(Thread.isMainThread)
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let method = DeviceMethod(ledger: directory.appendingPathComponent("settled"))
        await method.release.signal()
        let store = EnrollmentStore()
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
        XCTAssertFalse(method.mainThreadCalls.isEmpty)
        XCTAssertFalse(method.mainThreadCalls.contains(true))
        try await wallet.shutdown()
        try await fixture.finish()
    }
}
