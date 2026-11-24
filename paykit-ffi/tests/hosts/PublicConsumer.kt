package com.synonym.paykit.tests

import com.synonym.paykit.*

// The generated Kotlin calls for Public Payment Endpoints and the outbox, on the wallet `driveConsumers` enrolled.
internal suspend fun checkPublic(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.identity().contentEquals(fixture.identity()))
}
