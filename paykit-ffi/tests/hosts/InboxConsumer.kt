package com.synonym.paykit.tests

import com.synonym.paykit.*

// A payer can read its inbox and needs the extension scope to send (Paykit spec 3.3).
internal suspend fun checkInbox(fixture: CheckoutFixture, wallet: NativeWallet) {
    val payee = fixture.payee()
    check(wallet.spamCount(payee) == 0uL)
    check(wallet.extensionMessages().isEmpty())
    wallet.acknowledgeExtensionMessage(payee, "00000000-0000-4000-8000-000000000001")
    try {
        wallet.sendExtension(payee, "paykit-cap.offer", listOf(ByteArray(32)), "{}")
        error("sent without the extension scope")
    } catch (error: WalletException.NeedsUser) {
        error.failure.use { failure ->
            check(failure.code() == "scope_missing")
            checkNotNull(failure.refusal()).use { refusal ->
                val missing = refusal.kind() as RefusalKind.ScopeMissing
                check(missing.scope == "paykit_ext:paykit-cap")
            }
        }
    }
    check(wallet.extensionMessages().isEmpty())
}
