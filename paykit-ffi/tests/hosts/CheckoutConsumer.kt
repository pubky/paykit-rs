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

private suspend fun NativeWallet.followLoss(fixture: CheckoutFixture, second: Boolean) {
    val before = receive().generation
    fixture.replaceLostProvider(second)
    repeat(12) {
        val read = receive()
        if (read.generation > before && !read.stopped && !read.more) return
    }
    error("wallet did not follow the replacement provider")
}

private suspend fun NativeWallet.lossReport(): WalletReconciliation {
    repeat(12) {
        try {
            return reconcile()
        } catch (error: WalletException.Refused) {
            if (error.refusal.kind() !is RefusalKind.NotNow) throw error
        }
    }
    error("reconciliation did not finish its listing")
}

private suspend fun NativeWallet.reviewLoss(
    fixture: CheckoutFixture, request: RequestId, method: CheckoutMethod
) {
    suspend fun shown(): DoubtfulPayment {
        val report = lossReport()
        val row = report.doubtful.single { it.requestHash.contentEquals(request.requestHash) }
        check(!report.payerSkipped && row.payee.contentEquals(request.payee) && row.attempts.isEmpty())
        check(row.obligation.size == 32 && row.beforeLoss != null)
        return row
    }
    val first = shown()
    followLoss(fixture, true)
    val current = shown()
    endConfirmedReconciliation()
    check(current.obligation.contentEquals(first.obligation))
    val earlier = checkNotNull(first.beforeLoss)
    val latest = checkNotNull(current.beforeLoss)
    check(latest.generation > earlier.generation && latest.uncertainUntilMs >= latest.declaredAtMs)
    try {
        confirmNotPaidBeforeLoss(checkNotNull(earlier.payment))
        error("the earlier loss was accepted")
    } catch (error: WalletException.Refused) {
        val changed = checkNotNull((error.refusal.kind() as RefusalKind.LossChanged).boundary)
        check(changed.generation == latest.generation && changed.declaredAtMs == latest.declaredAtMs &&
            changed.uncertainUntilMs == latest.uncertainUntilMs && changed.payment == null)
    }
    val refused = payRequest(request, PaymentOptions(null, null, false, false, emptyList())).decision as PaymentDecision.Error
    val doubt = checkNotNull(((refused.error as WalletException.Refused).refusal.kind() as RefusalKind.MayBePaid).doubt)
    check(doubt.obligation.contentEquals(first.obligation) && doubt.beforeLoss?.generation == latest.generation)
    check(method.calls.get() == 0)
    confirmNotPaidBeforeLoss(checkNotNull(latest.payment))
    check(reconcile().doubtful.none { it.obligation.contentEquals(current.obligation) })
    allowConnect(fixture.payee())
}

private enum class CheckoutReview { ORDINARY, DIVERGENCE, PROVIDER_LOSS }

private fun driveCheckout(recovery: CheckoutReview) = runBlocking {
    withTimeout(60_000) {
        val directory = Files.createTempDirectory("paykit-method").toFile()
        try {
            val method = CheckoutMethod(File(directory, "settled"))
            val store = EnrollmentStore()
            enrolledTestCheckout(store, method).use { fixture ->
                check(fixture.allowanceRecoveryNoticeKinds() == listOf(
                    WalletNoticeKind.ALLOWANCE_ACCEPTANCE_BEFORE_LOSS,
                    WalletNoticeKind.ALLOWANCE_WITHDRAWAL_CONFIRMED
                ))
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
                    var shown: Divergence? = null
                    val request = host.load(enrolled.appId).use { originalWallet ->
                        var wallet = originalWallet
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
                        if (recovery == CheckoutReview.PROVIDER_LOSS) {
                            wallet.followLoss(fixture, false)
                            check(wallet.lossReport().doubtful.isEmpty())
                            wallet.endConfirmedReconciliation()
                            wallet.allowConnect(fixture.payee())
                        }
                        val sent = fixture.sendCheckout()
                        wallet.finishReceive()
                        val view = wallet.requests().single()
                        check(view.id.requestHash.contentEquals(sent.requestHash) && view.id.payee.contentEquals(sent.payee))
                        check(view.paymentReference == "checkout-7" && view.amount.value == "7")
                        val options = PaymentOptions(null, null, false, false, emptyList())
                        val missing = RequestId(sent.payee, ByteArray(32))
                        val skipped = wallet.payRequest(missing, options)
                        val failure = skipped.decision as PaymentDecision.Error
                        val refused = failure.error as WalletException.Refused
                        refused.refusal.use { check(it.kind() is RefusalKind.NotFound) }
                        check(skipped.publicFailures.isEmpty() && method.calls.get() == 0)
                        when (recovery) {
                            CheckoutReview.DIVERGENCE -> {
                                wallet.shutdown()
                                fixture.markDivergent(enrolled.appId, view.record)
                                wallet = host.load(enrolled.appId)
                                val held = wallet.payRequest(sent, options).decision as PaymentDecision.Error
                                val refusedRecord = ((held.error as WalletException.Refused).refusal.kind() as RefusalKind.Divergent).records.single()
                                val listed = wallet.divergentRecords().single()
                                check(refusedRecord.record.contentEquals(view.record) && listed.record.contentEquals(view.record))
                                check(refusedRecord.version != null && refusedRecord.version == listed.version && refusedRecord.paused &&
                                    refusedRecord.divergent && !refusedRecord.mayHoldCredit && method.calls.get() == 0)
                                shown = refusedRecord
                                wallet.confirmDivergent(refusedRecord.observation)
                                wallet.finishReceive()
                                check(wallet.divergentRecords().none { it.paused || it.writesWaiting })
                            }
                            CheckoutReview.PROVIDER_LOSS -> wallet.reviewLoss(fixture, sent, method)
                            CheckoutReview.ORDINARY -> Unit
                        }
                        wallet.finishReceive()
                        withTimeout(15_000) {
                            val watch = async { wallet.feedWait(25u) }
                            fixture.waitForHeldFeed()
                            check(watch.isActive)
                            val payment = async { wallet.payRequest(view.id, options) }
                            try {
                                check(watch.await() == FeedWake.READY)
                                method.entered.await()
                                payment.cancelAndJoin()
                                check(payment.isCancelled)
                            } finally {
                                payment.cancel()
                                watch.cancel()
                                method.release.complete(Unit)
                            }
                        }
                        wallet.shutdown()
                        sent
                    }
                    host.load(enrolled.appId).use { opened ->
                        var payer = opened
                        val original = payer.payments().single()
                        check(original.stage == PaymentStage.PAYING && original.amount.value == "7")
                        val resumed = payer.resumePayments()
                        check(resumed.any { it is PaymentOutcome.Paid && it.attemptId == original.attemptId })
                        shown?.let { old ->
                            payer.shutdown()
                            fixture.markDivergent(enrolled.appId, old.record)
                            payer = host.load(enrolled.appId)
                        }
                        val receipt = fixture.confirmPayment(request)
                        payer.finishReceive()
                        shown?.let { old ->
                            val latest = payer.divergentRecords().single()
                            check(latest.record.contentEquals(old.record) && latest.version != old.version)
                            try {
                                payer.confirmDivergent(old.observation)
                                error("the earlier divergent write was accepted")
                            } catch (error: WalletException.Operation) {
                                check(error.kind == WalletFailureKind.CONFLICT)
                            }
                            check(payer.divergentRecords().single().paused)
                            payer.confirmDivergent(latest.observation)
                            check(payer.divergentRecords().single().writesWaiting)
                            payer.finishReceive()
                            check(payer.divergentRecords().none { it.writesWaiting })
                        }
                        check(payer.requestReceipts(request).single().let {
                            it.messageId == receipt && it.attemptId == original.attemptId && it.paymentReference == "checkout-7" &&
                                it.payee.contentEquals(request.payee) && it.requestHash.contentEquals(request.requestHash)
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
    @JvmStatic fun main(args: Array<String>) = CheckoutReview.entries.forEach { driveCheckout(it) }
}
