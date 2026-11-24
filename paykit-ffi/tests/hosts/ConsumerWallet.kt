package com.synonym.paykit.tests

import com.synonym.paykit.*
import kotlinx.coroutines.*
import java.io.File
import java.nio.file.Files

// One enrolled payer wallet, the test service's payee allowed to connect, handed to each group
// of wallet calls in turn. Each group's checks live in its own file, and leave the wallet as
// they found it for the next.
internal fun driveConsumers() = runBlocking {
    withTimeout(120_000) {
        val directory = Files.createTempDirectory("paykit-consumers").toFile()
        try {
            val method = CheckoutMethod(File(directory, "settled"))
            val store = EnrollmentStore()
            enrolledTestCheckout(store, method, testDeclaration, testEndpoints).use { fixture ->
                method.clock = { fixture.nowMs() }
                val reference = ByteArray(16) { 31 }
                val secret = ByteArray(32) { 9 }
                WalletEnrollment(store).use { owner ->
                    val info = owner.beginEnrollment(reference)
                    val original = owner.prepareEnrollment(reference, "wallet.example", ByteArray(32) { 8 }, info.appRecordPath + ":rw", secret)
                    owner.acceptApproval(reference, fixture.identity(), fixture.approve(original.signedRequest, secret))
                }
                fixture.host().use { host ->
                    host.completeEnrollment(reference).use { wallet ->
                        wallet.allowConnect(fixture.payee())
                        checkInbox(fixture, wallet)
                        checkPayer(fixture, wallet)
                        checkPayee(fixture, wallet)
                        checkLists(fixture, wallet)
                        checkPublic(fixture, wallet)
                        checkPeriods(fixture, wallet)
                        checkAllowances(fixture, wallet)
                        checkExecutor(fixture, wallet)
                        wallet.shutdown()
                    }
                }
                fixture.finish()
            }
        } finally {
            directory.deleteRecursively()
        }
    }
}
