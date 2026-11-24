package com.synonym.paykit.tests

import com.synonym.paykit.*
import kotlinx.coroutines.*
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.File
import java.io.FileOutputStream
import java.nio.file.Files
import java.util.concurrent.atomic.AtomicInteger

internal open class CheckoutMethod(private val ledger: File) : ReceiptReader() {
    val entered = CompletableDeferred<Unit>()
    val release = CompletableDeferred<Unit>()
    val calls = AtomicInteger()
    @Volatile var pendingLookup = false
    /// The test services' clock, which dates each payment as a real method's evidence would.
    @Volatile var clock: (() -> ULong)? = null
    @Volatile private var paidAt: ULong? = null
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
        paidAt = checkNotNull(clock)()
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
            proof.endpoint == "btc-lightning-bolt11" && (proof.payload == null || proof.payload == "lnbc1checkout")) paidAt else null

    override fun settlementId(endpoint: String, proofJson: ByteArray): ByteArray? =
        stored()?.takeIf { proofJson.contentEquals(originalProof(it)) }?.toByteArray()
}

// Read until the feed is done and every record the app's scopes read is listed (Paykit spec 6).
internal suspend fun NativeWallet.finishReceive() {
    repeat(8) {
        val report = receive()
        if (!report.more && report.recordsUnlisted == null) return
    }
    error("wallet did not complete its feed")
}

private suspend fun restoreCheckout(
    fixture: CheckoutFixture, source: WalletHost, app: ByteArray, reference: ByteArray,
    method: CheckoutMethod, action: String,
) = coroutineScope {
    check(action in setOf("confirm", "refuse", "doubt"))
    val options = PaymentOptions(null, null, false, false, emptyList())
    val (backup, original) = source.completeEnrollment(reference).use { wallet ->
        wallet.finishReceive()
        wallet.allowConnect(fixture.payee())
        val original = fixture.sendCheckout()
        wallet.finishReceive()
        val snapshot = wallet.backup()
        fixture.passRotationInterval()
        wallet.finishReceive()
        if (action == "doubt") {
            // The host stops waiting and the wallet closes while the host's pay still runs:
            // after the pay time limit the attempt stays recorded as possibly sent.
            val payment = async { wallet.payRequest(original, options) }
            method.entered.await()
            payment.cancelAndJoin()
            method.pendingLookup = true
            wallet.shutdown()
            method.release.complete(Unit)
        } else wallet.shutdown()
        snapshot to original
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
            check(state.toString() == "WalletRestoreState(<redacted>)")
            try {
                wallet.confirmRestoredRegistrations(emptyList())
                error("the stale restore question was accepted")
            } catch (error: WalletException.NeedsUser) {
                checkNotNull(error.failure.refusal()).use { check(it.kind() is RefusalKind.NotNow) }
            }
            check(wallet.restoreState().registrations == state.registrations)
            suspend fun interruptConfirmation(failure: EnrollmentStore.AnswerFailure) {
                destination.interruptAnswer(failure)
                try {
                    wallet.confirmRestoredRegistrations(state.registrations)
                    error("registration answer unexpectedly reported a durable result")
                } catch (error: WalletException.Storage) {
                    if (failure == EnrollmentStore.AnswerFailure.UNAVAILABLE) check(error.failure is StorageException.Unavailable)
                    else check(error.failure is StorageException.Unknown)
                }
            }
            if (action == "refuse") {
                interruptConfirmation(EnrollmentStore.AnswerFailure.UNKNOWN_BEFORE_COMMIT)
                try {
                    wallet.refuseRestoredRegistrations(state.registrations)
                    error("the unrecognized registration did not stop the wallet")
                } catch (_: WalletException.Recovery) {}
                val alarm = wallet.restoreState().alarm as SwitchAlarm.Compromise
                check(alarm.n == 1uL && method.calls.get() == 0)
                try {
                    wallet.confirmRestoredRegistrations(state.registrations)
                    error("a stopped registration was confirmed")
                } catch (_: WalletException.Recovery) {}
                check(wallet.restoreState().alarm == alarm)
            } else {
                if (action == "confirm") {
                    interruptConfirmation(EnrollmentStore.AnswerFailure.UNAVAILABLE)
                    check(wallet.restoreState().registrations == state.registrations)
                    interruptConfirmation(EnrollmentStore.AnswerFailure.UNKNOWN_BEFORE_COMMIT)
                    // Retry the same answer on the same handle without an intervening read.
                    interruptConfirmation(EnrollmentStore.AnswerFailure.UNKNOWN_AFTER_COMMIT)
                    check(wallet.restoreState().registrations.isEmpty())
                    try {
                        wallet.confirmRestoredRegistrations(state.registrations)
                        error("an already committed answer was confirmed again")
                    } catch (error: WalletException) {
                        checkNotNull(error.categorized?.refusal())
                    }
                } else wallet.confirmRestoredRegistrations(state.registrations)
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
                wallet.finishReceive()
                val request = wallet.requests().single { it.id.requestHash.contentEquals(original.requestHash) }
                val refused = wallet.payRequest(original, options).decision as PaymentDecision.Error
                val confirmation = checkNotNull((refused.error as WalletException.NeedsUser).failure.refusal())
                val hold = confirmation.kind() as RefusalKind.HeldAfterRestore
                check(hold.record.contentEquals(request.record) && method.calls.get() == 0)
                method.release.complete(Unit)
                val result = wallet.payRequest(original, options.copy(confirmedRefusals = listOf(confirmation)))
                val paid = (result.decision as PaymentDecision.Outcome).outcome as PaymentOutcome.Paid
                val receipt = fixture.confirmPayment(original)
                wallet.finishReceive()
                val paidReceipt = wallet.requestReceipts(original).single()
                check(paidReceipt.messageId == receipt && paidReceipt.attemptId == paid.attemptId)
                check(method.calls.get() == 1)
                wallet.shutdown()
            }
        }
    }
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
        } catch (error: WalletException.NeedsUser) {
            if (error.failure.refusal()?.kind() !is RefusalKind.NotNow) throw error
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
    } catch (error: WalletException.NeedsUser) {
        check(error.failure.code() == "loss_changed")
        val changed = checkNotNull((checkNotNull(error.failure.refusal()).kind() as RefusalKind.LossChanged).boundary)
        check(changed.generation == latest.generation && changed.declaredAtMs == latest.declaredAtMs &&
            changed.uncertainUntilMs == latest.uncertainUntilMs && changed.payment == null)
    }
    val refused = payRequest(request, PaymentOptions(null, null, false, false, emptyList())).decision as PaymentDecision.Error
    val doubt = checkNotNull((checkNotNull((refused.error as WalletException.NeedsUser).failure.refusal()).kind() as RefusalKind.MayBePaid).doubt)
    check(doubt.obligation.contentEquals(first.obligation) && doubt.beforeLoss?.generation == latest.generation)
    check(method.calls.get() == 0)
    confirmNotPaidBeforeLoss(checkNotNull(latest.payment))
    check(reconcile().doubtful.none { it.obligation.contentEquals(current.obligation) })
    allowConnect(fixture.payee())
}

private enum class CheckoutReview { ORDINARY, DIVERGENCE, PROVIDER_LOSS }

private fun driveCheckout(recovery: CheckoutReview, restore: String? = null) = runBlocking {
    withTimeout(60_000) {
        val directory = Files.createTempDirectory("paykit-method").toFile()
        try {
            val method = CheckoutMethod(File(directory, "settled"))
            val store = EnrollmentStore()
            enrolledTestCheckout(store, method, testDeclaration, testEndpoints).use { fixture ->
                method.clock = { fixture.nowMs() }
                check(fixture.allowanceRecoveryNoticeKinds() == listOf(
                    WalletNoticeKind.ALLOWANCE_ACCEPTANCE_BEFORE_LOSS,
                    WalletNoticeKind.ALLOWANCE_WITHDRAWAL_CONFIRMED
                ))
                val reference = ByteArray(16) { 23 }
                val secret = ByteArray(32) { 9 }
                val enrolled = WalletEnrollment(store).use { owner ->
                    val info = owner.beginEnrollment(reference)
                    val original = owner.prepareEnrollment(reference, "wallet.example", ByteArray(32) { 8 }, info.appRecordPath + ":rw", secret, WalletScopes(pay = true))
                    owner.acceptApproval(reference, fixture.identity(), fixture.approve(original.signedRequest, secret))
                    info
                }
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
                        val refused = failure.error as WalletException.Permanent
                        check(refused.failure.code() == "not_found")
                        checkNotNull(refused.failure.refusal()).use { check(it.kind() is RefusalKind.NotFound) }
                        check(skipped.publicFailures.isEmpty() && method.calls.get() == 0)
                        when (recovery) {
                            CheckoutReview.DIVERGENCE -> {
                                wallet.shutdown()
                                fixture.markDivergent(enrolled.appId, view.record)
                                wallet = host.load(enrolled.appId)
                                val held = wallet.payRequest(sent, options).decision as PaymentDecision.Error
                                val refusedRecord = (checkNotNull((held.error as WalletException.NeedsUser).failure.refusal()).kind() as RefusalKind.Divergent).records.single()
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
                                // The host stops waiting; the payment itself goes on and is
                                // recorded once the host's pay answers.
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
                        check(original.stage in setOf(PaymentStage.RAIL_PAID, PaymentStage.ACCEPTANCE_OWED, PaymentStage.FINISHED))
                        check(original.paidByMethod)
                        check(original.amount.value == "7")
                        payer.resumePayments()
                        check(method.calls.get() == 1)
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
                            } catch (error: WalletException.NotSent) {
                                check(error.failure.code() == "conflict")
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

/// A method whose synchronous callbacks throw: a declared failure or any other exception.
private class ThrowingMethod(private val declared: Boolean) : ReceiptReader() {
    private fun fail(): Nothing =
        if (declared) throw MethodException.Unknown("callback failed") else error("callback crashed")
    override fun selectEndpoints(candidates: List<EndpointCandidate>, amount: Amount?): SelectedEndpoints = fail()
    override fun transfer(endpoint: String, payload: String, amount: Amount): MethodTransfer = fail()
    override fun settlementId(endpoint: String, proofJson: ByteArray): ByteArray? = fail()
}

/// A method that names the settlement of every proof.
private class SettlingMethod : ReceiptReader() {
    override fun settlementId(endpoint: String, proofJson: ByteArray): ByteArray? = "a1".toByteArray()
}

/// A callback that throws reads as no answer: no endpoint, an amount to confirm, no settlement.
private fun throwingCallbacks() {
    val proof = """{"settlement":"a1"}""".toByteArray()
    val answered = testMethodAnswers(SettlingMethod(), testDeclaration, testEndpoints, proof)
    check(answered.selected == listOf(1u) && answered.selectionFailure == null)
    check(answered.transferAmount?.value == "7" && answered.settlementId?.contentEquals("a1".toByteArray()) == true)
    for (declared in listOf(true, false)) {
        val thrown = testMethodAnswers(ThrowingMethod(declared), testDeclaration, testEndpoints, proof)
        check(thrown.selected == null && thrown.selectionFailure == "method_unknown")
        check(thrown.transferAmount == null && thrown.settlementId == null)
    }
}

/// A host whose callbacks die with an Error, not an Exception: the store, the Homeserver and
/// the payment method.
private class DyingCallbacks : ReceiptReader(), WalletStore {
    private fun die(): Nothing = throw AssertionError("callback died")
    override fun get(namespace: String, key: ByteArray): ByteArray? = die()
    override fun scan(namespace: String, prefix: ByteArray, after: ByteArray?, limit: UInt): List<StoreEntry> = die()
    override fun acquire(namespace: String, owner: ByteArray): Boolean = die()
    override fun release(namespace: String, owner: ByteArray) = die()
    override fun commitIf(namespace: String, owner: ByteArray, writes: List<StoreWrite>): Boolean = die()
    override suspend fun get(identity: ByteArray, path: String): ByteArray? = die()
    override suspend fun put(identity: ByteArray, path: String, bytes: ByteArray) = die()
    override fun selectEndpoints(candidates: List<EndpointCandidate>, amount: Amount?): SelectedEndpoints = die()
    override fun transfer(endpoint: String, payload: String, amount: Amount): MethodTransfer = die()
    override fun settlementId(endpoint: String, proofJson: ByteArray): ByteArray? = die()
    override suspend fun pay(payment: MethodPayment): MethodOutcome = die()
    override suspend fun payment(attemptId: String): MethodOutcome? = die()
}

/// A callback that dies with an Error answers as unknown at once, never as success: a store
/// write isn't read as committed or released, and a `pay` stays possibly sent. The platform
/// still reports each Error: JNA's callback handler for a sync callback, the default uncaught
/// handler for an async one.
private fun dyingCallbacks() = runBlocking {
    val died = { e: Throwable -> e is AssertionError && e.message == "callback died" }
    val synced = AtomicInteger()
    val awaited = AtomicInteger()
    val previousCallbackHandler = com.sun.jna.Native.getCallbackExceptionHandler()
    val previousThreadHandler = Thread.getDefaultUncaughtExceptionHandler()
    com.sun.jna.Native.setCallbackExceptionHandler { callback, e ->
        if (died(e)) synced.incrementAndGet() else previousCallbackHandler.uncaughtException(callback, e)
    }
    Thread.setDefaultUncaughtExceptionHandler { thread, e ->
        if (died(e)) awaited.incrementAndGet() else previousThreadHandler?.uncaughtException(thread, e)
    }
    try {
        val host = DyingCallbacks()
        val proof = """{"settlement":"a1"}""".toByteArray()
        val answered = testMethodAnswers(host, testDeclaration, testEndpoints, proof)
        check(answered.selected == null && answered.selectionFailure == "method_unknown")
        check(answered.transferAmount == null && answered.settlementId == null)
        check(testCallbackAnswers(host, host, host) == listOf(
            "store.get=unknown", "store.acquire=unknown", "store.commit_if=unknown", "store.release=unknown",
            "home.get=unknown", "home.put=unknown", "method.pay=possibly_sent", "method.payment=unknown",
        ))
        // Every callback ran and reported its Error: three of the method's and four of the
        // store's sync calls, two of the Homeserver's and two of the method's async calls.
        withTimeout(10_000) {
            while (synced.get() < 7 || awaited.get() < 4) delay(10)
        }
        check(synced.get() == 7 && awaited.get() == 4)
    } finally {
        com.sun.jna.Native.setCallbackExceptionHandler(previousCallbackHandler)
        Thread.setDefaultUncaughtExceptionHandler(previousThreadHandler)
    }
}

/// Text the switch gave never leaves the bindings: not in a message, a description or a reason.
private fun outsideTextStaysInside() {
    val sentinel = "SENTINEL-outside-text"
    try {
        testOutsideTextError(sentinel)
        error("no error")
    } catch (error: WalletException) {
        val failure = checkNotNull(error.categorized)
        val shown = listOf(
            error.message.orEmpty(), error.toString(), error.localizedMessage.orEmpty(),
            failure.code(), failure.context(), checkNotNull(failure.refusal()).reason(),
        )
        check(shown.none { it.contains(sentinel) }) { "outside text left the bindings" }
        check(failure.context() == "switch_unavailable: a later call can succeed")
    }
}

object CheckoutConsumer {
    @JvmStatic fun main(args: Array<String>) {
        val restore = System.getenv("PAYKIT_TEST_RESTORE")
        if (restore == null) outsideTextStaysInside()
        if (restore == null) throwingCallbacks()
        if (restore == null) dyingCallbacks()
        if (restore == null) CheckoutReview.entries.forEach { driveCheckout(it) }
        if (restore == null) driveConsumers()
        else driveCheckout(CheckoutReview.ORDINARY, restore)
    }
}
