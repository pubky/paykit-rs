package com.synonym.paykit.tests

import com.synonym.paykit.*

// The generated Kotlin calls for the one-time payer's calls, on the wallet `driveSlices` enrolled.
internal suspend fun checkPayer(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.identity().contentEquals(fixture.identity()))
}
