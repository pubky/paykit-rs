package com.synonym.paykit.tests

import com.synonym.paykit.*

// The generated Kotlin calls for the spam count and extension messages, on the wallet `driveConsumers` enrolled.
internal suspend fun checkInbox(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.identity().contentEquals(fixture.identity()))
}
