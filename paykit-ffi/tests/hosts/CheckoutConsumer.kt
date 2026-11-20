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
    @Volatile var pendingLookup = false
    fun chargedAttempt(): String = checkNotNull(stored())

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
        if (stored() != attemptId) null else if (pendingLookup) MethodOutcome.Pending else outcome(attemptId)

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

private suspend fun restoreCheckout(
    fixture: CheckoutFixture, source: WalletHost, app: ByteArray, reference: ByteArray,
    method: CheckoutMethod, action: String,
) = coroutineScope {
    check(action in setOf("confirm", "refuse", "doubt"))
    val options = PaymentOptions(null, null, false, false, emptyList())
    val backup = source.completeEnrollment(reference).use { wallet ->
        wallet.finishReceive()
        wallet.allowConnect(fixture.payee())
        val original = fixture.sendCheckout()
        wallet.finishReceive()
        val snapshot = wallet.backup()
        fixture.passRotationInterval()
        wallet.finishReceive()
        if (action == "doubt") {
            val payment = async { wallet.payRequest(original, options) }
            method.entered.await()
            payment.cancelAndJoin()
            method.release.complete(Unit)
            method.pendingLookup = true
        }
        wallet.shutdown()
        snapshot
    }
    val destination = EnrollmentStore()
    fixture.hostForStore(destination).use { host ->
        try {
            host.restore(app, backup, BackupDevice.UNACCOUNTED_FOR)
            error("an unaccounted source device was restored")
        } catch (_: WalletException.SourceDeviceUnaccountedFor) {}
        check(destination.untouched())
        host.restore(app, backup, BackupDevice.IN_POSSESSION).use { wallet ->
            check(wallet.appId().contentEquals(app))
            check(wallet.restoreState().reconciling)
            var asked = false
            repeat(8) {
                if (!asked) asked = wallet.receive().events.any {
                    (it.data as? WalletEventData.RestoreQuestion)?.n == listOf(1uL)
                }
            }
            check(asked && wallet.restoreState().registrations == listOf(1uL))
            wallet.shutdown()
        }
        host.load(app).use { wallet ->
            val state = wallet.restoreState()
            check(state.reconciling && state.cursor != null && state.registrations == listOf(1uL))
            try {
                wallet.confirmRestoredRegistrations(emptyList())
                error("the stale restore question was accepted")
            } catch (_: WalletException.Refused) {}
            check(wallet.restoreState().registrations == state.registrations)
            if (action == "refuse") {
                try {
                    wallet.refuseRestoredRegistrations(state.registrations)
                    error("the unrecognized registration did not stop the wallet")
                } catch (_: WalletException.Operation) {}
                val alarm = wallet.restoreState().alarm as SwitchAlarm.Compromise
                check(alarm.n == 1uL && method.calls.get() == 0)
            } else {
                wallet.confirmRestoredRegistrations(state.registrations)
                wallet.finishReceive()
                val report = wallet.reconcile()
                check(!report.payerSkipped && wallet.restoreState().registrations.isEmpty())
                if (action == "doubt") {
                    val charged = method.chargedAttempt()
                    val doubt = report.doubtful.single { charged in it.attempts }
                    wallet.resolveConfirmedAttempt(doubt.obligation, charged, ConfirmedResolution.PAID)
                    check(wallet.reconcile().doubtful.isEmpty() && method.calls.get() == 1)
                } else {
                    check(report.doubtful.isEmpty() && method.calls.get() == 0)
                }
                wallet.endConfirmedReconciliation()
                check(!wallet.restoreState().reconciling)
            }
            wallet.shutdown()
        }
        if (action == "confirm") {
            host.load(app).use { wallet ->
                check(!wallet.restoreState().reconciling)
                val next = fixture.sendCheckout()
                wallet.finishReceive()
                method.release.complete(Unit)
                val result = wallet.payRequest(next, options)
                val paid = (result.decision as PaymentDecision.Outcome).outcome as PaymentOutcome.Paid
                val receipt = fixture.confirmPayment(next)
                wallet.finishReceive()
                val paidReceipt = wallet.requestReceipts(next).single()
                check(paidReceipt.messageId == receipt && paidReceipt.attemptId == paid.attemptId)
                check(method.calls.get() == 1)
                wallet.shutdown()
            }
        }
    }
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
                val restore = System.getenv("PAYKIT_TEST_RESTORE")
                if (restore != null) {
                    fixture.host().use { host -> restoreCheckout(fixture, host, enrolled.appId, reference, method, restore) }
                    fixture.finish()
                    return@withTimeout
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
                fixture.finish()
            }
        } finally {
            directory.deleteRecursively()
        }
    }
}

object CheckoutConsumer {
    @JvmStatic fun main(args: Array<String>) = driveCheckout()
}
