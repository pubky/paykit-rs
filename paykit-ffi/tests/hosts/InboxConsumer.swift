import Foundation
import Paykit

/// A payer can read its inbox and needs the extension scope to send (Paykit spec 3.3).
func checkInbox(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let payee = fixture.payee()
    let count = try await wallet.spamCount(sender: payee)
    let messages = try await wallet.extensionMessages()
    precondition(count == 0 && messages.isEmpty)
    try await wallet.acknowledgeExtensionMessage(sender: payee, msgId: "00000000-0000-4000-8000-000000000001")
    let id = try await wallet.sendExtension(to: payee, kind: "paykit-cap.offer", refs: [Data(repeating: 0, count: 32)], bodyJson: "{}")
    precondition(UUID(uuidString: id) != nil)
    let remaining = try await wallet.extensionMessages()
    precondition(remaining.isEmpty)
}

func checkClosedInbox(wallet: NativeWallet, peer: Data) async throws {
    func closed<T>(_ call: () async throws -> T) async throws {
        do {
            _ = try await call()
            preconditionFailure("inbox call succeeded after shutdown")
        } catch WalletError.Closed {}
    }
    try await closed { try await wallet.spamCount(sender: peer) }
    try await closed { try await wallet.extensionMessages() }
    try await closed { try await wallet.acknowledgeExtensionMessage(sender: peer, msgId: "00000000-0000-4000-8000-000000000001") }
    try await closed { try await wallet.sendExtension(to: peer, kind: "paykit-cap.offer", refs: [], bodyJson: "{}") }
}
