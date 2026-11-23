import Foundation
import Paykit

/// The generated Swift calls for the Allowance lifecycle, on the wallet `driveSlices` enrolled.
func checkAllowances(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let identity = try await wallet.identity()
    precondition(identity == fixture.identity())
}
