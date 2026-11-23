package com.synonym.paykit.v2.tests

import android.os.Looper
import android.os.StrictMode
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.synonym.paykit.v2.*
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

/**
 * A full checkout driven from the main thread, on Paykit's 4 MiB threads, which this device
 * must accept. Strict mode ends the process on disk or network work on the main thread, and
 * the test records the thread of every store call and synchronous method callback.
 */
@RunWith(AndroidJUnit4::class)
class WalletOnDeviceTest {
    @Test
    fun test_wallet_checkout_from_the_main_thread_runs_off_it() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val ledger = File(instrumentation.targetContext.cacheDir, "paykit-method")
        ledger.deleteRecursively()
        check(ledger.mkdirs())
        // Load the native library before the main thread may not read the disk.
        WalletEnrollment(EnrollmentStore()).close()
        val done = CompletableDeferred<Unit>()
        instrumentation.runOnMainSync {
            StrictMode.setThreadPolicy(StrictMode.ThreadPolicy.Builder().detectAll().penaltyDeath().build())
            MainScope().launch {
                try {
                    checkoutFromMain(ledger)
                    done.complete(Unit)
                } catch (failure: Throwable) {
                    done.completeExceptionally(failure)
                }
            }
        }
        runBlocking { withTimeout(180_000) { done.await() } }
    }
}

/** The threads Paykit called the host's synchronous callbacks on. */
private val callbackThreads = java.util.Collections.synchronizedList(mutableListOf<Thread>())
private fun recorded() { callbackThreads += Thread.currentThread() }

private class DeviceMethod(ledger: File) : CheckoutMethod(ledger) {
    override fun declaration(): MethodDeclaration = super.declaration().also { recorded() }
    override fun selectEndpoints(candidates: List<EndpointCandidate>, amount: Amount?): SelectedEndpoints =
        super.selectEndpoints(candidates, amount).also { recorded() }
    override fun transfer(endpoint: String, payload: String, amount: Amount): MethodTransfer =
        super.transfer(endpoint, payload, amount).also { recorded() }
    override fun fractionDigits(endpoint: String): UInt? = super.fractionDigits(endpoint).also { recorded() }
    override fun settlementId(proofJson: ByteArray): ByteArray? = super.settlementId(proofJson).also { recorded() }
}

private class DeviceStore(private val inner: EnrollmentStore) : WalletStore {
    override fun get(namespace: String, key: ByteArray): ByteArray? = inner.get(namespace, key).also { recorded() }
    override fun scan(namespace: String, prefix: ByteArray, after: ByteArray?, limit: UInt): List<StoreEntry> =
        inner.scan(namespace, prefix, after, limit).also { recorded() }
    override fun acquire(namespace: String, owner: ByteArray): Boolean = inner.acquire(namespace, owner).also { recorded() }
    override fun release(namespace: String, owner: ByteArray) = inner.release(namespace, owner).also { recorded() }
    override fun commitIf(namespace: String, owner: ByteArray, writes: List<StoreWrite>): Boolean =
        inner.commitIf(namespace, owner, writes).also { recorded() }
}

private suspend fun checkoutFromMain(directory: File) {
    check(Looper.myLooper() == Looper.getMainLooper())
    val method = DeviceMethod(File(directory, "settled"))
    method.release.complete(Unit)
    val store = DeviceStore(EnrollmentStore())
    enrolledTestCheckout(store, method).use { fixture ->
        method.clock = { fixture.nowMs() }
        val reference = ByteArray(16) { 23 }
        val secret = ByteArray(32) { 9 }
        WalletEnrollment(store).use { owner ->
            val info = owner.beginEnrollment(reference)
            val request = owner.prepareEnrollment(reference, "wallet.example", ByteArray(32) { 8 }, info.appRecordPath + ":rw", secret)
            owner.acceptApproval(reference, fixture.identity(), fixture.approve(request.signedRequest, secret))
        }
        fixture.host().use { host ->
            host.completeEnrollment(reference).use { wallet ->
                wallet.finishReceive()
                wallet.allowConnect(fixture.payee())
                val sent = fixture.sendCheckout()
                wallet.finishReceive()
                val result = wallet.payRequest(sent, PaymentOptions(null, null, false, false, emptyList()))
                val paid = (result.decision as PaymentDecision.Outcome).outcome as PaymentOutcome.Paid
                val receipt = fixture.confirmPayment(sent)
                wallet.finishReceive()
                check(wallet.requestReceipts(sent).single().let { it.messageId == receipt && it.attemptId == paid.attemptId })
                check(method.calls.get() == 1)
                // Every store and synchronous method callback ran on Paykit's threads.
                check(callbackThreads.isNotEmpty() && callbackThreads.none { it == Looper.getMainLooper().thread })
                wallet.shutdown()
            }
        }
        fixture.finish()
    }
}
