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
 * A full checkout driven from the main thread. Strict mode ends the process on any disk or
 * network work there, so every wallet call, store and method callback has to run on Paykit's
 * own threads, whose 4 MiB stacks this device must accept.
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

/** Records the threads Paykit calls the method's synchronous callbacks on. */
private class DeviceMethod(ledger: File) : CheckoutMethod(ledger) {
    val threads = java.util.Collections.synchronizedList(mutableListOf<Thread>())
    override fun selectEndpoints(candidates: List<EndpointCandidate>, amount: Amount?): SelectedEndpoints {
        threads += Thread.currentThread()
        return super.selectEndpoints(candidates, amount)
    }
}

private suspend fun checkoutFromMain(directory: File) {
    check(Looper.myLooper() == Looper.getMainLooper())
    val method = DeviceMethod(File(directory, "settled"))
    method.release.complete(Unit)
    val store = EnrollmentStore()
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
                check(method.threads.isNotEmpty() && method.threads.none { it == Looper.getMainLooper().thread })
                wallet.shutdown()
            }
        }
        fixture.finish()
    }
}
