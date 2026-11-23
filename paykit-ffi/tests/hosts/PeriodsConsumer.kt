package com.synonym.paykit.tests

import com.synonym.paykit.*

// The generated Kotlin calls for recurring requests and Billing Periods, on the wallet `driveSlices` enrolled.
internal suspend fun checkPeriods(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.identity().contentEquals(fixture.identity()))
}
