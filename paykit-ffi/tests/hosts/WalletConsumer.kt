package com.synonym.paykit.tests

import com.synonym.paykit.*
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.util.Base64
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Enrollment needs only the store. A Homeserver or payment call here fails the test. */
internal class EnrollmentStore : WalletStore {
    enum class AnswerFailure { UNAVAILABLE, UNKNOWN_BEFORE_COMMIT, UNKNOWN_AFTER_COMMIT }
    private var answerFailure: AnswerFailure? = null

    @Synchronized fun interruptAnswer(failure: AnswerFailure) {
        check(answerFailure == null)
        answerFailure = failure
    }

    fun reopen(namespace: String, entries: List<String>) {
        for (line in entries) {
            val (key, value) = line.split(" ", limit = 2)
            rows[namespace to key] = Base64.getDecoder().decode(value)
        }
    }

    @Synchronized fun untouched() = rows.isEmpty() && acquisitions == 0
    private var acquisitions = 0
    private val owners = mutableMapOf<String, ByteArray>()
    private val rows = mutableMapOf<Pair<String, String>, ByteArray>()
    private var failingRead: String? = null
    @Volatile private var slowRelease: String? = null
    val releasing = CountDownLatch(1)
    val proceed = CountDownLatch(1)

    @Synchronized fun failReadAndHoldRelease(namespace: String) {
        failingRead = namespace
        slowRelease = namespace
    }

    @Synchronized fun hasWriter(namespace: String) = owners.containsKey(namespace)
    private fun key(namespace: String, key: ByteArray) =
        namespace to Base64.getEncoder().encodeToString(key)

    @Synchronized override fun get(namespace: String, key: ByteArray): ByteArray? {
        if (failingRead == namespace) {
            failingRead = null
            throw StorageException.Unavailable()
        }
        return rows[key(namespace, key)]?.copyOf()
    }

    @Synchronized override fun scan(namespace: String, prefix: ByteArray, after: ByteArray?, limit: UInt): List<StoreEntry> =
        rows.entries.asSequence()
            .filter { it.key.first == namespace }
            .map { StoreEntry(Base64.getDecoder().decode(it.key.second), it.value.copyOf()) }
            .filter { it.key.size >= prefix.size && prefix.indices.all { i -> prefix[i] == it.key[i] } }
            .filter { after == null || java.util.Arrays.compareUnsigned(it.key, after) > 0 }
            .sortedWith { a, b -> java.util.Arrays.compareUnsigned(a.key, b.key) }
            .take(limit.toLong().coerceAtMost(Int.MAX_VALUE.toLong()).toInt()).toList()

    @Synchronized override fun acquire(namespace: String, owner: ByteArray): Boolean {
        acquisitions++
        return owners.putIfAbsent(namespace, owner.copyOf()) == null
    }

    override fun release(namespace: String, owner: ByteArray) {
        if (slowRelease == namespace) {
            releasing.countDown()
            check(proceed.await(10, TimeUnit.SECONDS))
            slowRelease = null
        }
        synchronized(this) {
            if (owners[namespace]?.contentEquals(owner) == true) owners.remove(namespace)
        }
    }

    @Synchronized override fun commitIf(namespace: String, owner: ByteArray, writes: List<StoreWrite>): Boolean {
        if (owners[namespace]?.contentEquals(owner) != true) return false
        val failure = answerFailure?.takeIf {
            val keys = writes.filterIsInstance<StoreWrite.Put>().map { it.key.decodeToString() }
            keys.any { it.startsWith("reg/") } && keys.any { it.startsWith("kid/") }
        }
        if (failure != null) answerFailure = null
        if (failure == AnswerFailure.UNAVAILABLE) throw StorageException.Unavailable()
        if (failure == AnswerFailure.UNKNOWN_BEFORE_COMMIT) throw StorageException.Unknown()
        for (write in writes) {
            when (write) {
                is StoreWrite.Put -> rows[key(namespace, write.key)] = write.value.copyOf()
                is StoreWrite.Delete -> rows.remove(key(namespace, write.key))
                is StoreWrite.DeletePrefix -> rows.keys.removeIf { (ns, encoded) ->
                    val k = Base64.getDecoder().decode(encoded)
                    ns == namespace && k.size >= write.prefix.size && write.prefix.indices.all { write.prefix[it] == k[it] }
                }
            }
        }
        if (failure == AnswerFailure.UNKNOWN_AFTER_COMMIT) throw StorageException.Unknown()
        return true
    }
}

internal open class ReceiptReader : WalletHomeserver, PaymentMethod {
    var transfers = 0
    override suspend fun get(identity: ByteArray, path: String): ByteArray? = null
    override suspend fun put(identity: ByteArray, path: String, bytes: ByteArray) = error("unexpected public write")
    override suspend fun delete(identity: ByteArray, path: String) = error("unexpected public delete")
    override fun declaration() = MethodDeclaration(true, null, true, null, true, 3600000uL)
    override fun selectEndpoints(candidates: List<EndpointCandidate>, amount: Amount?): SelectedEndpoints {
        val (unavailable, usable) = candidates.partition { it.identifier == "btc-lightning-bolt12" }
        return SelectedEndpoints(usable.asReversed().map { it.id }, unavailable.map {
            EndpointFailure(it.id, MethodException.Unavailable("route unavailable"))
        })
    }
    override fun transfer(endpoint: String, payload: String, amount: Amount) = MethodTransfer(amount, null)
    override fun fractionDigits(endpoint: String): UInt? = 8u
    override suspend fun pay(payment: MethodPayment): MethodOutcome {
        transfers += 1
        error("a receipted payment cannot be dispatched again")
    }
    override suspend fun payment(attemptId: String): MethodOutcome? = null
    override suspend fun allowanceHistory(payer: ByteArray): List<MethodDispatch>? = null
    override suspend fun verify(proof: MethodProof): ULong? = null
    override fun settlementId(proofJson: ByteArray): ByteArray? = null
}

private fun walletRows(issuer: String, mode: String): List<String> {
    val producer = ProcessBuilder(issuer, mode).redirectError(ProcessBuilder.Redirect.INHERIT).start()
    val values = producer.inputStream.bufferedReader().readLines()
    check(producer.waitFor() == 0)
    return values
}

private suspend fun resumeRefusal(issuer: String, left: Boolean, request: RequestId) {
    val values = walletRows(issuer, if (left) "departed-wallet" else "wallet-awaiting-identity")
    val store = EnrollmentStore().apply { reopen(values[1], values.drop(2)) }
    val method = ReceiptReader()
    WalletHost(store, method, method, listOf("btc-lightning-bolt11")).use { host ->
        host.load(Base64.getDecoder().decode(values[0])).use { wallet ->
            if (!left) {
                try {
                    wallet.requestReceipts(request)
                    error("read receipts before the restored records were listed")
                } catch (error: WalletException.Refused) {
                    error.refusal.use { check(it.kind() is RefusalKind.NotNow) }
                }
            }
            try {
                wallet.resumePayments()
                error("resumed a held wallet")
            } catch (error: WalletException.Refused) {
                check(!left)
                error.refusal.use { refusal ->
                    check(refusal.kind() is RefusalKind.IdentityUnavailable)
                    check(refusal.attemptedFailure() == null)
                }
            } catch (error: WalletException.Operation) {
                check(left && error.kind == WalletFailureKind.CLIENT)
                checkNotNull(error.switchRefusal).use { check(it.kind() is SwitchRefusalKind.Left) }
            }
            check(method.transfers == 0)
            wallet.shutdown()
        }
    }
}

private suspend fun recoverReceipt(issuer: String): RequestId {
    val values = walletRows(issuer, "paid-wallet")
    check(values.size > 7)
    fun bytes(i: Int): ByteArray = Base64.getDecoder().decode(values[i])
    val app = bytes(0)
    val identity = bytes(1)
    val request = RequestId(bytes(2), bytes(3))
    val original = EnrollmentStore().apply { reopen(values[6], values.drop(7)) }
    val method = ReceiptReader()
    WalletHost(original, method, method, listOf("btc-lightning-bolt11")).use { device ->
        device.load(app).use { wallet ->
            check(wallet.appId().contentEquals(app) && wallet.identity().contentEquals(identity))
            check(wallet.backup().isNotEmpty())
            val receipt = wallet.requestReceipts(request).single()
            val entry = wallet.payments().single()
            val periodUsage = AllowancePeriodUsage(entry.amount.value, 1uL)
            val selection = method.selectEndpoints(listOf(
                EndpointCandidate(6u, "btc-onchain", "address"),
                EndpointCandidate(2u, "btc-lightning-bolt12", "offer"),
                EndpointCandidate(9u, entry.endpoint, "invoice"),
            ), entry.amount)
            val failure = selection.failures.single()
            val unavailable = failure.error as MethodException.Unavailable
            check(selection.payable == listOf(9u, 6u) && failure.id == 2u && unavailable.reason == "route unavailable")
            check(unavailable.message == "MethodException(<redacted>)")
            val walletError = WalletException.Operation(WalletFailureKind.CLIENT, "operation failed", null, byteArrayOf(1), null)
            check(walletError.message == "WalletException(<redacted>)" && walletError.detail == "operation failed")
            val unknown: WalletException = WalletException.UnknownMethod()
            check(unknown is WalletException.UnknownMethod && unknown.toString() == "WalletException(<redacted>)")
            val descriptions: List<Pair<Any, String>> = listOf(
                entry to "PaymentEntry", receipt to "Receipt", request to "RequestId", entry.amount to "Amount",
                BillingPeriod(1uL, 2uL) to "BillingPeriod",
                PaymentOutcome.Paid(entry.attemptId, null) to "PaymentOutcome",
                MethodDispatch(entry.attemptId, entry.amount, MethodOutcome.Pending) to "MethodDispatch",
                periodUsage to "AllowancePeriodUsage",
                AllowanceUsage(entry.amount.value, listOf(periodUsage), 0uL, 1uL, 0uL) to "AllowanceUsage",
                selection to "SelectedEndpoints", failure to "EndpointFailure", unavailable to "MethodException",
                method.declaration() to "MethodDeclaration", walletError to "WalletException",
            )
            for ((value, name) in descriptions) check(value.toString() == "$name(<redacted>)")
            check(entry.amount.value == "7" && entry.attemptId == values[4])
            check(entry.receipt?.messageId == receipt.messageId)
            check(receipt.attemptId == values[4] && receipt.messageId == values[5])
            check(receipt.paymentReference == "checkout-7" && method.transfers == 0)
            wallet.shutdown()
        }
        coroutineScope {
            original.failReadAndHoldRelease(values[6])
            val failed = async {
                try {
                    device.load(app).close()
                    error("failed read opened a wallet")
                } catch (error: WalletException.Storage) {
                    check(error.failure is StorageException.Unavailable && !original.hasWriter(values[6]))
                }
            }
            check(withContext(Dispatchers.IO) { original.releasing.await(5, TimeUnit.SECONDS) })
            try {
                device.load(app).close()
                error("opened during release")
            } catch (error: WalletException.Storage) {
                check(error.failure is StorageException.InUse)
            } finally {
                original.proceed.countDown()
            }
            failed.await()
            device.load(app).use { reopened ->
                check(reopened.requestReceipts(request).single().messageId == values[5])
                reopened.shutdown()
            }
        }
    }
    return request
}

private suspend fun acknowledgeRecovery(issuer: String, cleared: Boolean) {
    val mode = if (cleared) "cleared-wallet" else "notice-wallet"
    val producer = ProcessBuilder(issuer, mode).redirectError(ProcessBuilder.Redirect.INHERIT).start()
    val values = producer.inputStream.bufferedReader().readLines()
    check(producer.waitFor() == 0 && values.size > 6)
    val app = Base64.getDecoder().decode(values[0])
    val generation = values[1].toULong()
    val duty = Base64.getDecoder().decode(values[4])
    val store = EnrollmentStore().apply { reopen(values[5], values.drop(6)) }
    val method = ReceiptReader()
    WalletHost(store, method, method, listOf("btc-lightning-bolt11")).use { host ->
        host.load(app).use { wallet ->
            val event = wallet.unacknowledgedEvents().single { it.id.contentEquals(duty) }
            check(event.owners == listOf("paykit"))
            check(event.toString() == "WalletEvent(<redacted>)")
            check(event.data.toString() == "WalletEventData(<redacted>)")
            if (cleared) {
                check(wallet.recoveryRequired() == null)
                check((event.data as WalletEventData.RecoveryNoticeCleared).generation == generation)
            } else {
                val notice = checkNotNull(wallet.recoveryRequired())
                check((event.data as WalletEventData.RecoveryRequired).notice == notice)
                check(notice.generation == generation && notice.reason == RecoveryReason.BEHIND && !notice.permanent)
                check(notice.firstSeenMs == values[2].toULong() && notice.serviceTimeMs == values[3].toULong())
            }
            wallet.acknowledgeEvents(duty)
            wallet.shutdown()
        }
        host.load(app).use { wallet ->
            check(wallet.unacknowledgedEvents().none { it.id.contentEquals(duty) })
            check(wallet.recoveryRequired()?.generation == if (cleared) null else generation)
            check(method.transfers == 0)
            wallet.shutdown()
        }
    }
}

fun main() = runBlocking {
    val store = EnrollmentStore()
    val reference = ByteArray(16) { 7 }
    val clientKey = ByteArray(32) { 8 }
    val secret = ByteArray(32) { 9 }
    fun host() = WalletEnrollment(store)
    val original = host().use { wallet ->
        val entry = wallet.beginEnrollment(reference)
        try {
            wallet.prepareEnrollment(reference, "wallet.example", clientKey, "/pub/another.json:rw", secret)
            error("invalid app capabilities were pinned")
        } catch (error: WalletException.InvalidInput) { check(error.field == "capabilities") }
        wallet.prepareEnrollment(reference, "wallet.example", clientKey, entry.appRecordPath + ":rw", secret)
    }
    host().use { wallet ->
        val entry = wallet.beginEnrollment(reference)
        check(entry.appId.contentEquals(original.appId))
        val caps = entry.appRecordPath + ":rw"
        val resumed = wallet.prepareEnrollment(reference, "wallet.example", clientKey, caps, secret)
        check(resumed.requestHash.contentEquals(original.requestHash))
        check(resumed.signedRequest.contentEquals(original.signedRequest))
        check(resumed.toString() == "EnrollmentRequest(<redacted>)")
        try {
            wallet.prepareEnrollment(reference, "wallet.example", clientKey, caps, ByteArray(32) { 10 })
            error("changed secret accepted")
        } catch (_: WalletException.EnrollmentConflict) { }
    }
    check(StoreWrite.Put(byteArrayOf(1), secret).toString() == "StoreWrite(<redacted>)")
    val issuer = ProcessBuilder(checkNotNull(System.getenv("PAYKIT_TEST_ISSUER")))
        .redirectError(ProcessBuilder.Redirect.INHERIT).start()
    issuer.outputStream.use { it.write(original.signedRequest) }
    val response = issuer.inputStream.bufferedReader().readLines()
    check(issuer.waitFor() == 0 && response.size == 2)
    val identity = Base64.getDecoder().decode(response[0])
    val claim = Base64.getDecoder().decode(response[1])
    host().use { it.acceptApproval(reference, identity, claim) }
    host().use { it.acceptApproval(reference, identity, claim) }
    try {
        host().use { it.acceptApproval(reference, identity, byteArrayOf(1)) }
        error("replaced the stored approval")
    } catch (_: WalletException.EnrollmentConflict) {}
    val path = checkNotNull(System.getenv("PAYKIT_TEST_ISSUER"))
    val request = recoverReceipt(path)
    for (left in listOf(false, true)) resumeRefusal(path, left, request)
    acknowledgeRecovery(path, false)
    acknowledgeRecovery(path, true)
}
