package com.synonym.paykit.v2.tests

import com.synonym.paykit.v2.*
import kotlinx.coroutines.runBlocking
import java.util.Base64

/** Enrollment needs only the store. A Homeserver or payment call here fails the test. */
private class EnrollmentStore : WalletStore {
    private val owners = mutableMapOf<String, ByteArray>()
    private val rows = mutableMapOf<Pair<String, String>, ByteArray>()
    private fun key(namespace: String, key: ByteArray) =
        namespace to Base64.getEncoder().encodeToString(key)

    @Synchronized override fun get(namespace: String, key: ByteArray) =
        rows[key(namespace, key)]?.copyOf()

    @Synchronized override fun acquire(namespace: String, owner: ByteArray): Boolean =
        owners.putIfAbsent(namespace, owner.copyOf()) == null

    @Synchronized override fun release(namespace: String, owner: ByteArray) {
        if (owners[namespace]?.contentEquals(owner) == true) owners.remove(namespace)
    }

    @Synchronized override fun commitIf(namespace: String, owner: ByteArray, writes: List<StoreWrite>): Boolean {
        if (owners[namespace]?.contentEquals(owner) != true) return false
        val put = writes.single() as StoreWrite.Put
        rows[key(namespace, put.key)] = put.value.copyOf()
        return true
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
}
