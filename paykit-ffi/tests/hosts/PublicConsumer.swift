import Foundation
import Paykit

/// The generated Swift calls for Public Payment Endpoints and the outbox, on the wallet `driveConsumers` enrolled.
func checkPublic(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let identity = try await wallet.identity()
    precondition(identity == fixture.identity())
}
