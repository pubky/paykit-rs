package com.synonym.paykit.tests

import com.synonym.paykit.*

// Share Private Payment Lists and confirm another list after an unknown delivery (Paykit spec 8.4.1).
internal suspend fun checkLists(fixture: CheckoutFixture, wallet: NativeWallet) {
    val recipient = fixture.payee()
    val endpoint = ListEndpoint(identifier = "btc-lightning-bolt11", payload = "lnbc1example")
    wallet.setListEndpoints(listOf(endpoint))
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
        check(error.field == "refused.message_hash")
    }
    wallet.acknowledgePublicationConflict(report)
    check(wallet.publicationConflicts().isEmpty())

    fixture.restoredListSeller().use { seller ->
        val payer = wallet.identity()
        val original = try {
            seller.publishListTo(payer)
            error("sent a replacement without confirmation")
        } catch (error: WalletException.NeedsUser) {
            error.failure.use { failure ->
                checkNotNull(failure.refusal()).use { refusal ->
                    val shown = refusal.kind() as RefusalKind.ListDeliveryUnknown
                    check(shown.recipient.contentEquals(payer) && !shown.withdrawal)
                    seller.sendListAgain(refusal)
                    shown.msgId
                }
            }
        }
        val received = mutableListOf<String>()
        // A list can wait for a record listing even after the last feed page.
        for (read in 1..8) {
            val report = wallet.receive()
            for (event in report.events) {
                val message = event.data as? WalletEventData.MessageReceived ?: continue
                if (message.kind == "paykit.private_payment_list" && message.sender.contentEquals(recipient)) {
                    received.add(message.msgId)
                }
            }
            if (received.size >= 2) break
        }
        check(received.size == 2 && received.first() == original && received.last() != original)
        seller.shutdown()
    }
}
