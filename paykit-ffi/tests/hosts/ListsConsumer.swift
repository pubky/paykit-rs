import Foundation
import Paykit

/// Share Private Payment Lists and confirm another list after an unknown delivery (Paykit spec 8.4.1).
func checkLists(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let recipient = fixture.payee()
    let endpoint = ListEndpoint(identifier: "btc-lightning-bolt11", payload: "lnbc1example")
    try await wallet.setListEndpoints(endpoints: [endpoint])
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
        precondition(field == "refused.message_hash")
    }
    try await wallet.acknowledgePublicationConflict(conflict: report)
    let remaining = try await wallet.publicationConflicts()
    precondition(remaining.isEmpty)

    let seller = try await fixture.restoredListSeller()
    let payer = try await wallet.identity()
    let original: String
    do {
        try await seller.publishListTo(recipient: payer)
        preconditionFailure("sent a replacement without confirmation")
    } catch let WalletError.NeedsUser(failure) {
        guard let refusal = failure.refusal(),
              case let .listDeliveryUnknown(to, _, _, msgId, withdrawal, _) = refusal.kind(),
              to == payer, !withdrawal else { preconditionFailure("expected an unknown list delivery") }
        original = msgId
        try await seller.sendListAgain(refusal: refusal)
    }
    var received: [String] = []
    // A list can wait for a record listing even after the last feed page.
    for _ in 0..<8 {
        let report = try await wallet.receive()
        for event in report.events {
            if case let .messageReceived(_, _, kind, id, sender) = event.data,
               kind == "paykit.private_payment_list", sender == recipient {
                received.append(id)
            }
        }
        if received.count >= 2 { break }
    }
    precondition(received.count == 2 && received.first == original && received.last != original)
    try await seller.shutdown()
}
