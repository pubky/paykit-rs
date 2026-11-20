package com.synonym.paykit.v2.tests

import com.synonym.paykit.v2.*
import kotlinx.coroutines.runBlocking
import java.util.Base64

/** Enrollment needs only the store. A Homeserver or payment call here fails the test. */
private class EnrollmentStore : WalletStore {
    fun reopen(namespace: String, entries: List<String>) {
        for (line in entries) {
            val (key, value) = line.split(" ", limit = 2)
            rows[namespace to key] = Base64.getDecoder().decode(value)
        }
    }

    private val owners = mutableMapOf<String, ByteArray>()
    private val rows = mutableMapOf<Pair<String, String>, ByteArray>()
    private fun key(namespace: String, key: ByteArray) =
        namespace to Base64.getEncoder().encodeToString(key)

    @Synchronized override fun get(namespace: String, key: ByteArray) =
        rows[key(namespace, key)]?.copyOf()

    @Synchronized override fun scan(namespace: String, prefix: ByteArray, after: ByteArray?, limit: UInt): List<StoreEntry> =
        rows.entries.asSequence()
            .filter { it.key.first == namespace }
            .map { StoreEntry(Base64.getDecoder().decode(it.key.second), it.value.copyOf()) }
            .filter { it.key.size >= prefix.size && prefix.indices.all { i -> prefix[i] == it.key[i] } }
            .filter { after == null || java.util.Arrays.compareUnsigned(it.key, after) > 0 }
            .sortedWith { a, b -> java.util.Arrays.compareUnsigned(a.key, b.key) }
            .take(limit.toLong().coerceAtMost(Int.MAX_VALUE.toLong()).toInt()).toList()

    @Synchronized override fun acquire(namespace: String, owner: ByteArray): Boolean =
        owners.putIfAbsent(namespace, owner.copyOf()) == null

    @Synchronized override fun release(namespace: String, owner: ByteArray) {
        if (owners[namespace]?.contentEquals(owner) == true) owners.remove(namespace)
    }

    @Synchronized override fun commitIf(namespace: String, owner: ByteArray, writes: List<StoreWrite>): Boolean {
        if (owners[namespace]?.contentEquals(owner) != true) return false
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
        return true
    }
}

private class ReceiptReader : WalletHomeserver, PaymentMethod {
    var transfers = 0
    override suspend fun get(identity: ByteArray, path: String): ByteArray? = null
    override suspend fun put(identity: ByteArray, path: String, bytes: ByteArray) = error("unexpected public write")
    override suspend fun delete(identity: ByteArray, path: String) = error("unexpected public delete")
    override fun declaration() = MethodDeclaration(true, null, true, null, true, 3600000uL)
    override fun selectEndpoints(candidates: List<EndpointCandidate>, amount: Amount?) =
        SelectedEndpoints(candidates.map { it.id }, emptyList())
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

private suspend fun recoverReceipt(issuer: String) {
    val producer = ProcessBuilder(issuer, "paid-wallet").start()
    val values = producer.inputStream.bufferedReader().readLines()
    check(producer.waitFor() == 0 && values.size > 7)
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
            check(entry.amount.value == "7" && entry.attemptId == values[4])
            check(entry.receipt?.messageId == receipt.messageId)
            check(receipt.attemptId == values[4] && receipt.messageId == values[5])
            check(receipt.paymentReference == "checkout-7" && method.transfers == 0)
            wallet.shutdown()
        }
    }
}

private suspend fun acknowledgeRecovery(issuer: String, cleared: Boolean) {
    val mode = if (cleared) "cleared-wallet" else "notice-wallet"
    val producer = ProcessBuilder(issuer, mode).start()
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
    val issuer = ProcessBuilder(checkNotNull(System.getenv("PAYKIT_TEST_ISSUER"))).start()
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
    val issuerPath = checkNotNull(System.getenv("PAYKIT_TEST_ISSUER"))
    recoverReceipt(issuerPath)
    acknowledgeRecovery(issuerPath, false)
    acknowledgeRecovery(issuerPath, true)
}
