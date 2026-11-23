package com.synonym.paykit.tests

import com.synonym.paykit.*

// The generated Kotlin calls for payments under an Allowance, on the wallet `driveSlices` enrolled.
internal suspend fun checkExecutor(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.identity().contentEquals(fixture.identity()))
}
