package com.synonym.paykit.tests

import com.synonym.paykit.*

// The generated Kotlin calls for the payee's requests, payment checks and receipts, on the wallet `driveConsumers` enrolled.
internal suspend fun checkPayee(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.identity().contentEquals(fixture.identity()))
}
