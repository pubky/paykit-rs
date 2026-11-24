import Foundation
import Paykit

/// The generated Swift calls for recurring requests and Billing Periods, on the wallet `driveConsumers` enrolled.
func checkPeriods(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let identity = try await wallet.identity()
    precondition(identity == fixture.identity())
}
