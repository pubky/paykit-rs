package com.synonym.paykit.tests

import com.synonym.paykit.*

// A payer can inspect Private Payment Lists and acknowledge an old report (Paykit spec 8.4.1).
internal suspend fun checkLists(fixture: CheckoutFixture, wallet: NativeWallet) {
    val recipient = fixture.payee()
    val endpoint = ListEndpoint(identifier = "btc-lightning-bolt11", payload = "lnbc1example")
    try {
        wallet.setListEndpoints(listOf(endpoint))
        error("payer set a Private Payment List")
    } catch (error: WalletException.NeedsUser) {
        error.failure.use { failure ->
            check(failure.code() == "scope_missing")
            checkNotNull(failure.refusal()).use { refusal ->
                check((refusal.kind() as RefusalKind.ScopeMissing).scope == "paykit_request")
            }
        }
    }
    try {
        wallet.publishListTo(recipient)
        error("payer published a Private Payment List")
    } catch (error: WalletException.NeedsUser) {
        error.failure.use { failure ->
            check(failure.code() == "not_now")
            // Only an unknown list delivery is a confirmation to send another list.
            checkNotNull(failure.refusal()).use { refusal ->
                try {
                    wallet.sendListAgain(refusal)
                    throw AssertionError("a refusal other than an unknown delivery sent a list")
                } catch (invalid: WalletException.InvalidInput) {
                    check(invalid.field == "refusal")
                }
            }
        }
    }
    wallet.withdrawListFrom(recipient)
    check(wallet.heldLists().isEmpty())
    check(wallet.publicationConflicts().isEmpty())

    val held = ListPublicationRecord(
        publisher = wallet.appId(), recipient = recipient, generation = 1uL, counter = 2uL,
        msgId = "3198762f-3816-4cd4-86e2-4a8dc40fc737", messageHash = ByteArray(32) { 1 },
    )
    val refused = held.copy(
        counter = 1uL, msgId = "a5dbfc07-4d85-4b39-bd47-29ff28493ce1", messageHash = ByteArray(32) { 2 },
    )
    val report = ListPublicationConflict(ByteArray(32) { 3 }, false, held, refused)
    try {
        wallet.acknowledgePublicationConflict(report.copy(refused = refused.copy(messageHash = byteArrayOf())))
        error("acknowledged a malformed message hash")
    } catch (error: WalletException.InvalidInput) {
        check(error.field == "message_hash")
    }
    wallet.acknowledgePublicationConflict(report)
    check(wallet.publicationConflicts().isEmpty())
}
