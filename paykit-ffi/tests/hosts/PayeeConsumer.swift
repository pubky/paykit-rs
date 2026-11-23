import Foundation
import Paykit

/// The generated Swift calls for the payee's requests, payment checks and receipts, on the wallet `driveSlices` enrolled.
func checkPayee(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let identity = try await wallet.identity()
    precondition(identity == fixture.identity())
}
