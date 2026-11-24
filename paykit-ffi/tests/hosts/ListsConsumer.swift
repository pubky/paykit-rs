import Foundation
import Paykit

/// A payer can inspect Private Payment Lists and acknowledge an old report (Paykit spec 8.4.1).
func checkLists(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let recipient = fixture.payee()
    let endpoint = ListEndpoint(identifier: "btc-lightning-bolt11", payload: "lnbc1example")
    do {
        try await wallet.setListEndpoints(endpoints: [endpoint])
        preconditionFailure("payer set a Private Payment List")
    } catch let WalletError.NeedsUser(failure) {
        precondition(failure.code() == "scope_missing")
        guard case let .scopeMissing(scope)? = failure.refusal()?.kind() else {
            preconditionFailure("missing selling scope refusal")
        }
        precondition(scope == "paykit_request")
    }
    do {
        try await wallet.publishListTo(recipient: recipient)
        preconditionFailure("payer published a Private Payment List")
    } catch let WalletError.NeedsUser(failure) {
        precondition(failure.code() == "not_now")
    }
    try await wallet.withdrawListFrom(recipient: recipient)
    let lists = try await wallet.heldLists()
    let conflicts = try await wallet.publicationConflicts()
    precondition(lists.isEmpty && conflicts.isEmpty)

    let held = ListPublicationRecord(
        publisher: try await wallet.appId(), recipient: recipient, generation: 1, counter: 2,
        msgId: "3198762f-3816-4cd4-86e2-4a8dc40fc737", messageHash: Data(repeating: 1, count: 32)
    )
    var refused = held
    refused.counter = 1
    refused.msgId = "a5dbfc07-4d85-4b39-bd47-29ff28493ce1"
    refused.messageHash = Data(repeating: 2, count: 32)
    let report = ListPublicationConflict(
        applicationHash: Data(repeating: 3, count: 32), otherApplications: false, held: held, refused: refused
    )
    var malformed = report
    malformed.refused.messageHash = Data()
    do {
        try await wallet.acknowledgePublicationConflict(conflict: malformed)
        preconditionFailure("acknowledged a malformed message hash")
    } catch WalletError.InvalidInput(let field) {
        precondition(field == "message_hash")
    }
    try await wallet.acknowledgePublicationConflict(conflict: report)
    let remaining = try await wallet.publicationConflicts()
    precondition(remaining.isEmpty)
}
