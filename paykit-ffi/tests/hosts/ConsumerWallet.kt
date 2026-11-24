package com.synonym.paykit.tests

import com.synonym.paykit.*
import kotlinx.coroutines.*
import java.io.File
import java.nio.file.Files

// Run the wallet calls with the scopes approved for this app (Paykit spec 3.1).
internal fun driveConsumers() = runBlocking {
    withTimeout(120_000) {
        val directory = Files.createTempDirectory("paykit-consumers").toFile()
        try {
            val method = CheckoutMethod(File(directory, "settled"))
            val store = EnrollmentStore()
            val scopes = WalletScopes(pay = true, request = true, allowance = true, allowanceExecute = true, contacts = true, manageContacts = true,
                extensions = listOf("paykit-cap"))
            val config = WalletConfig(extensionKinds = listOf("paykit-cap.offer"), publishesRecord = true)
            enrolledTestCheckout(store, method, testDeclaration, testEndpoints, config).use { fixture ->
                method.clock = { fixture.nowMs() }
                val reference = ByteArray(16) { 31 }
                val secret = ByteArray(32) { 9 }
                WalletEnrollment(store).use { owner ->
                    val info = owner.beginEnrollment(reference)
                    val original = owner.prepareEnrollment(reference, "wallet.example", ByteArray(32) { 8 }, info.appRecordPath + ":rw", secret, scopes)
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
                        checkClosedInbox(wallet, fixture.payee())
                    }
                }
                fixture.finish()
            }
        } finally {
            directory.deleteRecursively()
        }
    }
}
