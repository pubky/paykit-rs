package com.synonym.paykit.tests

import com.synonym.paykit.*

// A payer can read its inbox and needs the extension scope to send (Paykit spec 3.3).
internal suspend fun checkInbox(fixture: CheckoutFixture, wallet: NativeWallet) {
    val payee = fixture.payee()
    check(wallet.spamCount(payee) == 0uL)
    check(wallet.extensionMessages().isEmpty())
    wallet.acknowledgeExtensionMessage(payee, "00000000-0000-4000-8000-000000000001")
    val id = wallet.sendExtension(payee, "paykit-cap.offer", listOf(ByteArray(32)), "{}")
    check(java.util.UUID.fromString(id).version() == 4)
    check(wallet.extensionMessages().isEmpty())
}

internal suspend fun checkClosedInbox(wallet: NativeWallet, peer: ByteArray) {
    suspend fun closed(call: suspend () -> Unit) {
        try {
            call()
            error("inbox call succeeded after shutdown")
        } catch (_: WalletException.Closed) {}
    }
    closed { wallet.spamCount(peer) }
    closed { wallet.extensionMessages() }
    closed { wallet.acknowledgeExtensionMessage(peer, "00000000-0000-4000-8000-000000000001") }
    closed { wallet.sendExtension(peer, "paykit-cap.offer", emptyList(), "{}") }
}
