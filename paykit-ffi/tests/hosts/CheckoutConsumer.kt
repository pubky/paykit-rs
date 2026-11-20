package com.synonym.paykit.v2.tests

import com.synonym.paykit.v2.*
import kotlinx.coroutines.*
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.File
import java.io.FileOutputStream
import java.nio.file.Files
import java.util.concurrent.atomic.AtomicInteger

private class CheckoutMethod(private val ledger: File) : ReceiptReader() {
    val entered = CompletableDeferred<Unit>()
    val release = CompletableDeferred<Unit>()
    val calls = AtomicInteger()

    private fun originalProof(attempt: String) = """{"settlement":"$attempt"}""".toByteArray()
    private fun outcome(attempt: String) = MethodOutcome.Paid(originalProof(attempt), null)

    @Synchronized private fun stored() = if (!ledger.exists()) null else
        ledger.inputStream().use { DataInputStream(it).readUTF() }

    @Synchronized private fun commit(payment: MethodPayment) {
        check(payment.amount.asset == "btc" && payment.amount.value == "7")
        check(payment.endpoint == "btc-lightning-bolt11" && payment.payload == "lnbc1checkout")
        check(payment.paymentReference == "checkout-7")
        val earlier = stored()
        if (earlier != null) {
            check(earlier == payment.attemptId)
            return
        }
        FileOutputStream(ledger).use { file ->
            DataOutputStream(file).writeUTF(payment.attemptId)
            file.fd.sync()
        }
    }

    override suspend fun pay(payment: MethodPayment): MethodOutcome {
        calls.incrementAndGet()
        commit(payment)
        entered.complete(Unit)
        release.await()
        return outcome(payment.attemptId)
    }
    override suspend fun payment(attemptId: String): MethodOutcome? =
        if (stored() == attemptId) outcome(attemptId) else null

    override suspend fun verify(proof: MethodProof): ULong? =
        if (proof.attemptId == stored() && proof.proofJson.contentEquals(originalProof(proof.attemptId)) &&
            proof.amount.asset == "btc" && proof.amount.value == "7" &&
            proof.endpoint == "btc-lightning-bolt11" && (proof.payload == null || proof.payload == "lnbc1checkout")) 1uL else null

    override fun settlementId(proofJson: ByteArray): ByteArray? =
        stored()?.takeIf { proofJson.contentEquals(originalProof(it)) }?.toByteArray()
}

private suspend fun NativeWallet.finishReceive() {
    repeat(8) {
        val report = receive()
        if (!report.more) return
    }
    error("wallet did not complete its feed")
}

private fun driveCheckout() = runBlocking {
    withTimeout(60_000) {
        val directory = Files.createTempDirectory("paykit-method").toFile()
        try {
            val method = CheckoutMethod(File(directory, "settled"))
            val store = EnrollmentStore()
            enrolledTestCheckout(store, method).use { fixture ->
                val reference = ByteArray(16) { 23 }
                val secret = ByteArray(32) { 9 }
                val enrolled = WalletEnrollment(store).use { owner ->
                    val info = owner.beginEnrollment(reference)
                    val original = owner.prepareEnrollment(reference, "wallet.example", ByteArray(32) { 8 }, info.appRecordPath + ":rw", secret)
                    owner.acceptApproval(reference, fixture.identity(), fixture.approve(original.signedRequest, secret))
                    info
                }
                fixture.host().use { host ->
                    val (notice, duty) = host.completeEnrollment(reference).use { wallet ->
                        wallet.finishReceive()
                        fixture.setAccountMissing(true)
                        val stopped = wallet.receive()
                        val notice = checkNotNull(wallet.recoveryRequired())
                        check(stopped.stopped)
                        val event = stopped.events.single {
                            (it.data as? WalletEventData.RecoveryRequired)?.notice == notice
                        }
                        check(event.owners == listOf("paykit"))
                        wallet.shutdown()
                        notice to checkNotNull(event.id)
                    }
                    val request = host.load(enrolled.appId).use { wallet ->
                        check(wallet.unacknowledgedEvents().any {
                            it.id.contentEquals(duty) && it.owners == listOf("paykit")
                        })
                        wallet.acknowledgeEvents(duty)
                        check(wallet.recoveryRequired() == notice)
                        fixture.setAccountMissing(false)
                        check(wallet.receive().events.any {
                            (it.data as? WalletEventData.RecoveryNoticeCleared)?.generation == notice.generation
                        })
                        wallet.allowConnect(fixture.payee())
                        val sent = fixture.sendCheckout()
                        wallet.finishReceive()
                        val view = wallet.requests().single()
                        check(view.id.requestHash.contentEquals(sent.requestHash) && view.paymentReference == "checkout-7")
                        val options = PaymentOptions(null, null, false, false, emptyList())
                        val payment = async { wallet.payRequest(view.id, options) }
                        method.entered.await()
                        payment.cancelAndJoin()
                        method.release.complete(Unit)
                        check(payment.isCancelled)
                        wallet.shutdown()
                        sent
                    }
                    host.load(enrolled.appId).use { payer ->
                        val original = payer.payments().single()
                        check(original.stage == PaymentStage.PAYING && original.amount.value == "7")
                        val resumed = payer.resumePayments()
                        check(resumed.any { it is PaymentOutcome.Paid && it.attemptId == original.attemptId })
                        val receipt = fixture.confirmPayment(request)
                        payer.finishReceive()
                        check(payer.requestReceipts(request).single().let {
                            it.messageId == receipt && it.attemptId == original.attemptId && it.paymentReference == "checkout-7"
                        })
                        check(payer.payments().single().receipt?.messageId == receipt && method.calls.get() == 1)
                        payer.shutdown()
                    }
                }
            }
        } finally {
            directory.deleteRecursively()
        }
    }
}

object CheckoutConsumer {
    @JvmStatic fun main(args: Array<String>) = driveCheckout()
}
