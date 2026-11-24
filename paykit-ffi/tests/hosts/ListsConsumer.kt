package com.synonym.paykit.tests

import com.synonym.paykit.*

// The generated Kotlin calls for Private Payment Lists, on the wallet `driveConsumers` enrolled.
internal suspend fun checkLists(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.identity().contentEquals(fixture.identity()))
}
