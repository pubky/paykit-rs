package com.synonym.paykit.tests

import com.synonym.paykit.*

// The generated Kotlin calls for the Allowance lifecycle, on the wallet `driveConsumers` enrolled.
internal suspend fun checkAllowances(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.identity().contentEquals(fixture.identity()))
}
