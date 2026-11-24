import Foundation
import Paykit

/// The generated Swift calls for the spam count and extension messages, on the wallet `driveConsumers` enrolled.
func checkInbox(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let identity = try await wallet.identity()
    precondition(identity == fixture.identity())
}
