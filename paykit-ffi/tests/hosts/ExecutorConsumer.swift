import Foundation
import Paykit

/// The generated Swift calls for payments under an Allowance, on the wallet `driveConsumers` enrolled.
func checkExecutor(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let identity = try await wallet.identity()
    precondition(identity == fixture.identity())
}
