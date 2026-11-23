import Foundation
import Paykit

/// One enrolled payer wallet, the test service's payee allowed to connect, handed to each group
/// of wallet calls in turn. Each group's checks live in its own file, and leave the wallet as
/// they found it for the next.
func driveSlices() async throws {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    let method = CheckoutMethod(ledger: directory.appendingPathComponent("settled"))
    let store = EnrollmentStore()
    let fixture = try await enrolledTestCheckout(store: store, method: method, declaration: testDeclaration, endpoints: testEndpoints)
    method.useClock { fixture.nowMs() }
    let reference = Data(repeating: 31, count: 16)
    let secret = Data(repeating: 9, count: 32)
    let owner = WalletEnrollment(store: store)
    let enrolled = try await owner.beginEnrollment(reference: reference)
    let original = try await owner.prepareEnrollment(reference: reference, clientId: "wallet.example", clientKey: Data(repeating: 8, count: 32), capabilities: enrolled.appRecordPath + ":rw", authSecret: secret)
    let approval = try await fixture.approve(request: original.signedRequest, secret: secret)
    try await owner.acceptApproval(reference: reference, identity: fixture.identity(), claim: approval)
    let wallet = try await fixture.host().completeEnrollment(reference: reference)
    try await wallet.allowConnect(payee: fixture.payee())
    try await checkInbox(fixture: fixture, wallet: wallet)
    try await checkPayer(fixture: fixture, wallet: wallet)
    try await checkPayee(fixture: fixture, wallet: wallet)
    try await checkLists(fixture: fixture, wallet: wallet)
    try await checkPublic(fixture: fixture, wallet: wallet)
    try await checkPeriods(fixture: fixture, wallet: wallet)
    try await checkAllowances(fixture: fixture, wallet: wallet)
    try await checkExecutor(fixture: fixture, wallet: wallet)
    try await wallet.shutdown()
    try await fixture.finish()
}
