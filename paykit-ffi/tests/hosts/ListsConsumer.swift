import Foundation
import Paykit

/// The generated Swift calls for Private Payment Lists, on the wallet `driveSlices` enrolled.
func checkLists(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let identity = try await wallet.identity()
    precondition(identity == fixture.identity())
}
