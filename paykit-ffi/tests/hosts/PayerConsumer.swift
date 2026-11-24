import Foundation
import Paykit

/// The generated Swift calls for the one-time payer's calls, on the wallet `driveConsumers` enrolled.
func checkPayer(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let identity = try await wallet.identity()
    precondition(identity == fixture.identity())
}
