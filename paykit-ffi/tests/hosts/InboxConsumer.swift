import Foundation
import Paykit

/// A payer can read its inbox and needs the extension scope to send (Paykit spec 3.3).
func checkInbox(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let payee = fixture.payee()
    let count = try await wallet.spamCount(sender: payee)
    let messages = try await wallet.extensionMessages()
    precondition(count == 0 && messages.isEmpty)
    try await wallet.acknowledgeExtensionMessage(sender: payee, msgId: "00000000-0000-4000-8000-000000000001")
    do {
        _ = try await wallet.sendExtension(to: payee, kind: "paykit-cap.offer", refs: [Data(repeating: 0, count: 32)], bodyJson: "{}")
        preconditionFailure("sent without the extension scope")
    } catch let WalletError.NeedsUser(failure) {
        precondition(failure.code() == "scope_missing")
        guard case let .scopeMissing(scope)? = failure.refusal()?.kind() else {
            preconditionFailure("missing extension scope expected")
        }
        precondition(scope == "paykit_ext:paykit-cap")
    }
    let remaining = try await wallet.extensionMessages()
    precondition(remaining.isEmpty)
}
