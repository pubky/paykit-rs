package com.synonym.paykit.tests

import com.synonym.paykit.*
import java.io.File
import java.util.Base64
import java.util.UUID

// Read the payee's records and check refusals on the enrolled wallet (Paykit spec 6, 8.1, 9).
internal suspend fun checkPayee(fixture: CheckoutFixture, wallet: NativeWallet) {
    // Payee reads wait for the wallet's first full read (Paykit spec 6).
    wallet.finishReceive()
    val peer = fixture.payee()
    val draft = RequestDraft(
        amount = Amount("btc", "7"),
        destination = RequestDestination.Endpoints(listOf(ListEndpoint("btc-lightning-bolt11", "lnbc1checkout"))),
        paymentReference = "checkout-7",
        metadataJson = """{"tax":1.25}""".toByteArray(),
    )
    suspend fun notSent(call: suspend () -> Unit) {
        try {
            call()
            error("invalid message was sent")
        } catch (error: WalletException.NotSent) {
            error.failure.use { check(it.code() == "invalid_input") }
        }
    }
    notSent { wallet.sendRequest(peer, draft.copy(acceptedEndpointIdentifiers = listOf("btc-onchain"))) }
    notSent { wallet.requestResend(peer, ResendSelection()) }

    suspend fun missing(call: suspend () -> Unit) {
        try {
            call()
            error("found a proof for an absent record")
        } catch (error: WalletException.Permanent) {
            error.failure.use { check(it.code() == "not_found") }
        }
    }
    val absent = ByteArray(32)
    missing { wallet.verifyPayment(absent) }
    missing { wallet.confirmPayment(absent, ReceiptDetails(storeOnly = true)) }
    check(wallet.payeeActivity().none { it.record.contentEquals(absent) })
    check(wallet.retainedPayeeReceipts().none { it.record.contentEquals(absent) })

    // Sending and cancelling leave signed history. Give those calls their own checkout (Paykit spec 8.1).
    val store = EnrollmentStore()
    // The shop pays through this method, which settles at once and verifies its own proofs.
    val ledger = File.createTempFile("payee", "settled").apply { delete(); deleteOnExit() }
    val method = CheckoutMethod(ledger).apply { release.complete(Unit) }
    enrolledTestCheckout(store, method, testDeclaration, testEndpoints).use { checkout ->
        method.clock = { checkout.nowMs() }
        val reference = ByteArray(16) { 31 }
        val secret = ByteArray(32) { 9 }
        val info = WalletEnrollment(store).use { owner ->
            val enrolled = owner.beginEnrollment(reference)
            val scopes = WalletScopes(pay = true, request = true, contacts = true, manageContacts = true,
                extensions = listOf("paykit-cap"))
            val original = owner.prepareEnrollment(reference, "wallet.example", ByteArray(32) { 8 },
                enrolled.appRecordPath + ":rw", secret, scopes)
            owner.acceptApproval(reference, checkout.identity(), checkout.approve(original.signedRequest, secret))
            enrolled
        }
        checkout.host().use { host ->
            host.completeEnrollment(reference).use { seller ->
                try {
                    seller.payeeActivity()
                    error("read activity before the first listing")
                } catch (error: WalletException.NeedsUser) {
                    error.failure.use { check(it.code() == "not_now") }
                }
                seller.finishReceive()
                checkSentRequest(checkout, seller, draft)
                seller.shutdown()
            }

            // An unreadable retained receipt fails the entire list (Paykit spec 6).
            val app = info.appRecordPath.substringAfterLast('/').removeSuffix(".json")
            val encoder = Base64.getEncoder()
            val key = encoder.encodeToString("p/paykit/rc/unreadable".toByteArray())
            val value = encoder.encodeToString("{}".toByteArray())
            store.reopen("client/$app", listOf("$key $value"))
            host.load(info.appId).use { seller ->
                try {
                    seller.retainedPayeeReceipts()
                    error("returned a partial receipt list")
                } catch (error: WalletException.Storage) {
                    check(error.failure is StorageException.Corrupt)
                }
                seller.shutdown()
            }
        }
        checkout.finish()
    }
}

private suspend fun checkSentRequest(fixture: CheckoutFixture, wallet: NativeWallet, draft: RequestDraft) {
    val peer = fixture.payee()
    val sent = wallet.sendRequest(peer, draft)
    val id = checkNotNull(sent.request)
    check(id.payee.contentEquals(fixture.identity()) && id.requestHash.size == 32)
    check(UUID.fromString(sent.msgId).version() == 4)
    check(sent.delivery == wallet.messageDelivery(sent.msgId))
    val row = wallet.payeeActivity().single { it.requestHash?.contentEquals(id.requestHash) == true }
    check(row.record.size == 32 && row.payer.contentEquals(peer) && row.amount == draft.amount)
    check(row.status == PayeeStatus.REQUESTED && row.attemptId == null && row.label == null)
    check(!row.divergent && !row.recurring && row.periods.isEmpty())

    val cancellation = wallet.cancelSentRequest(row.record, "withdrawn")
    check(UUID.fromString(cancellation).version() == 4 && cancellation != sent.msgId)
    check(wallet.payeeActivity().single { it.record.contentEquals(row.record) }.status == PayeeStatus.CANCELLED)
    try {
        wallet.cancelSentRequest(row.record, null)
        error("cancelled a request twice")
    } catch (error: WalletException.Permanent) {
        error.failure.use { check(it.code() == "cancelled") }
    }
    val resend = wallet.requestResend(peer, ResendSelection(
        acceptedFromMs = fixture.nowMs(), acceptedUntilMs = fixture.nowMs(), requestHashes = listOf(id.requestHash),
    ))
    check(UUID.fromString(resend).version() == 4 && resend != cancellation)
    check(wallet.messageDelivery(resend) is MessageDelivery.Accepted)

    // The shop pays a second request; the wallet verifies the proof and issues its receipt (Paykit spec 8.1).
    wallet.allowConnect(peer)
    val paid = checkNotNull(wallet.sendRequest(peer, draft).request)
    fixture.payRequest(paid)
    wallet.finishReceive()
    val proven = wallet.payeeActivity().single { it.requestHash?.contentEquals(paid.requestHash) == true }
    check(proven.status == PayeeStatus.PROOF_RECEIVED && proven.attemptId != null)
    check(wallet.verifyPayment(proven.record) != null)
    val receipt = checkNotNull(wallet.confirmPayment(proven.record, null))
    check(UUID.fromString(receipt).version() == 4)
    check(wallet.confirmPayment(proven.record, null) == receipt)
    check(wallet.payeeActivity().single { it.record.contentEquals(proven.record) }.status == PayeeStatus.RECEIPTED)
}
