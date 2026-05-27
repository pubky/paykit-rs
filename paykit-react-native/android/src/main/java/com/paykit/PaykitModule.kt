package com.paykit

import com.facebook.react.bridge.Arguments
import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactContextBaseJavaModule
import com.facebook.react.bridge.ReactMethod
import com.synonym.paykit.*
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject

class PaykitModule(reactContext: ReactApplicationContext) :
    ReactContextBaseJavaModule(reactContext) {

    override fun getName(): String {
        return NAME
    }

    private fun resultArray(value: String) = Arguments.createArray().apply {
        pushString("ok")
        pushString(value)
    }

    private fun errorArray(message: String) = Arguments.createArray().apply {
        pushString("error")
        pushString(message)
    }

    private fun entriesFromJsonArray(array: JSONArray): List<FfiPaymentEntry> {
        return List(array.length()) { index ->
            val item = array.getJSONObject(index)
            FfiPaymentEntry(
                paymentEndpointIdentifier = item.getString("payment_endpoint_identifier"),
                paymentEndpointPayload = item.getString("payment_endpoint_payload")
            )
        }
    }

    private fun entriesJsonArray(entries: List<FfiPaymentEntry>): JSONArray {
        return JSONArray().apply {
            entries.forEach { entry ->
                put(JSONObject().apply {
                    put("payment_endpoint_identifier", entry.paymentEndpointIdentifier)
                    put("payment_endpoint_payload", entry.paymentEndpointPayload)
                })
            }
        }
    }

    private fun entriesJson(entries: List<FfiPaymentEntry>): String {
        return entriesJsonArray(entries).toString()
    }

    private fun privatePaymentEnvelopeFromJson(json: String): FfiPrivatePaymentEnvelope {
        val envelope = JSONObject(json)
        return FfiPrivatePaymentEnvelope(
            reference = envelope.getString("reference"),
            entries = entriesFromJsonArray(envelope.getJSONArray("entries"))
        )
    }

    private fun privatePaymentEnvelopeJson(envelope: FfiPrivatePaymentEnvelope?): String {
        if (envelope == null) {
            return "null"
        }
        return JSONObject().apply {
            put("reference", envelope.reference)
            put("entries", entriesJsonArray(envelope.entries))
        }.toString()
    }

    private fun optionalString(item: JSONObject, key: String): String? {
        return if (!item.has(key) || item.isNull(key)) null else item.getString(key)
    }

    private fun optionalJsonArray(item: JSONObject, key: String): JSONArray? {
        if (!item.has(key) || item.isNull(key)) {
            return null
        }
        return item.optJSONArray(key)
            ?: throw IllegalArgumentException("$key must be a JSON array")
    }

    private fun metadataFromJsonArray(array: JSONArray?): List<FfiReceiptMetadataEntry> {
        if (array == null) {
            return emptyList()
        }
        return List(array.length()) { index ->
            val item = array.getJSONObject(index)
            FfiReceiptMetadataEntry(
                key = item.getString("key"),
                value = item.getString("value")
            )
        }
    }

    private fun metadataJsonArray(metadata: List<FfiReceiptMetadataEntry>): JSONArray {
        return JSONArray().apply {
            metadata.forEach { entry ->
                put(JSONObject().apply {
                    put("key", entry.key)
                    put("value", entry.value)
                })
            }
        }
    }

    private fun receiptDraftFromJson(json: String): FfiReceiptDraft {
        val draft = JSONObject(json)
        return FfiReceiptDraft(
            reference = draft.getString("reference"),
            paymentEndpointIdentifier = optionalString(draft, "payment_endpoint_identifier"),
            amount = optionalString(draft, "amount"),
            currency = optionalString(draft, "currency"),
            metadata = metadataFromJsonArray(optionalJsonArray(draft, "metadata"))
        )
    }

    private fun issuedReceiptJson(receipt: FfiIssuedReceipt): String {
        return JSONObject().apply {
            put("reference", receipt.reference)
            put("location", receipt.location)
            put("key", receipt.key)
        }.toString()
    }

    private fun receiptAccessJsonObject(access: FfiReceiptAccess): JSONObject {
        return JSONObject().apply {
            put("version", access.version.toLong())
            put("reference", access.reference)
            put("location", access.location)
            put("key", access.key)
            put("algorithm", access.algorithm)
        }
    }

    private fun receiptAccessJson(access: List<FfiReceiptAccess>): String {
        return JSONArray().apply {
            access.forEach { item ->
                put(receiptAccessJsonObject(item))
            }
        }.toString()
    }

    private fun receiptJson(receipt: FfiReceipt): String {
        return JSONObject().apply {
            put("reference", receipt.reference)
            put("recipient_public_key", receipt.recipientPublicKey)
            put("payment_endpoint_identifier", receipt.paymentEndpointIdentifier ?: JSONObject.NULL)
            put("amount", receipt.amount ?: JSONObject.NULL)
            put("currency", receipt.currency ?: JSONObject.NULL)
            put("metadata", metadataJsonArray(receipt.metadata))
        }.toString()
    }

    private fun progressJson(progress: FfiHandshakeProgress): String {
        return JSONObject().apply {
            put("status", progress.status)
            put("handle_id", progress.handleId)
        }.toString()
    }

    private fun uint32(value: Double, label: String): UInt {
        require(!value.isNaN() && !value.isInfinite() && value % 1.0 == 0.0 &&
            value >= 0.0 && value <= UInt.MAX_VALUE.toDouble()) {
            "$label must be an integer between 0 and ${UInt.MAX_VALUE}"
        }
        return value.toLong().toUInt()
    }

    // -----------------------------------------------------------------------
    // Initialization
    // -----------------------------------------------------------------------

    @ReactMethod
    fun initialize(promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                check(PaykitAndroid.initialize(reactApplicationContext)) {
                    "Failed to initialize Android platform verifier"
                }
                paykitInitialize()
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Session queries
    // -----------------------------------------------------------------------

    @ReactMethod
    fun isAuthenticated(promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val result = paykitIsAuthenticated()
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(if (result) "true" else "false"))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun getCurrentPublicKey(promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val result = paykitGetCurrentPublicKey()
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(result ?: ""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun exportSession(promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val result = paykitExportSession()
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(result))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Authentication
    // -----------------------------------------------------------------------

    @ReactMethod
    fun importSession(sessionSecret: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val result = paykitImportSession(sessionSecret)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(result))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun signUp(secretKeyHex: String, homeserverPublicKey: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val result = paykitSignUp(secretKeyHex, homeserverPublicKey)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(result))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun signIn(secretKeyHex: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val result = paykitSignIn(secretKeyHex)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(result))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun signOut(promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSignOut()
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun forceSignOut(promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitForceSignOut()
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Payment List (read)
    // -----------------------------------------------------------------------

    @ReactMethod
    fun getPaymentList(publicKey: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val entries = paykitGetPaymentList(publicKey)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(entriesJson(entries)))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun getPaymentEndpoint(publicKey: String, paymentEndpointIdentifier: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val result = paykitGetPaymentEndpoint(publicKey, paymentEndpointIdentifier)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(result ?: ""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Payment endpoints (write)
    // -----------------------------------------------------------------------

    @ReactMethod
    fun setPaymentEndpoint(paymentEndpointIdentifier: String, paymentEndpointPayload: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSetPaymentEndpoint(paymentEndpointIdentifier, paymentEndpointPayload)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun removePaymentEndpoint(paymentEndpointIdentifier: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitRemovePaymentEndpoint(paymentEndpointIdentifier)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Private encrypted payments
    // -----------------------------------------------------------------------

    @ReactMethod
    fun defaultMaxSendRetries(promise: Promise) {
        try {
            promise.resolve(resultArray(paykitDefaultMaxSendRetries().toString()))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun defaultMaxRecoveryAttempts(promise: Promise) {
        try {
            promise.resolve(resultArray(paykitDefaultMaxRecoveryAttempts().toString()))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun generatePaymentReference(promise: Promise) {
        try {
            promise.resolve(resultArray(paykitGeneratePaymentReference()))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun initiateEncryptedLink(secretKeyHex: String, receiverPublicKey: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val handle = paykitInitiateEncryptedLink(secretKeyHex, receiverPublicKey)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(handle))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun acceptEncryptedLink(secretKeyHex: String, senderPublicKey: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val handle = paykitAcceptEncryptedLink(secretKeyHex, senderPublicKey)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(handle))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun advanceHandshake(handshakeId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val progress = paykitAdvanceHandshake(handshakeId)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(progressJson(progress)))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun setEncryptedLinkHandshakeMaxRecoveryAttempts(handshakeId: String, max: Double, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSetEncryptedLinkHandshakeMaxRecoveryAttempts(
                    handshakeId,
                    uint32(max, "max recovery attempts")
                )
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun setEncryptedLinkMaxSendRetries(linkId: String, max: Double, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSetEncryptedLinkMaxSendRetries(
                    linkId,
                    uint32(max, "max send retries")
                )
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun setPrivatePaymentEnvelope(linkId: String, payloadJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSetPrivatePaymentEnvelope(linkId, privatePaymentEnvelopeFromJson(payloadJson))
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun getPrivatePaymentEnvelope(linkId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val envelope = paykitGetPrivatePaymentEnvelope(linkId)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(privatePaymentEnvelopeJson(envelope)))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun issueReceipt(linkId: String, draftJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val receipt = paykitIssueReceipt(linkId, receiptDraftFromJson(draftJson))
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(issuedReceiptJson(receipt)))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun getReceiptAccess(linkId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val access = paykitGetReceiptAccess(linkId)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(receiptAccessJson(access)))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun receiptLocation(reference: String, promise: Promise) {
        try {
            promise.resolve(resultArray(paykitReceiptLocation(reference)))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun decryptReceipt(encryptedJson: String, key: String, location: String, promise: Promise) {
        try {
            promise.resolve(resultArray(receiptJson(paykitDecryptReceipt(encryptedJson, key, location))))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun serializeEncryptedLinkHandshake(handshakeId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val snapshot = paykitSerializeEncryptedLinkHandshake(handshakeId)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(snapshot))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun serializeEncryptedLink(linkId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val snapshot = paykitSerializeEncryptedLink(linkId)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(snapshot))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun encryptedLinkSnapshotRecipient(snapshotHex: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val recipient = paykitEncryptedLinkSnapshotRecipient(snapshotHex)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(recipient))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun encryptedLinkHandshakeSnapshotRecipient(snapshotHex: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val recipient = paykitEncryptedLinkHandshakeSnapshotRecipient(snapshotHex)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(recipient))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun restoreEncryptedLink(secretKeyHex: String, snapshotHex: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val handle = paykitRestoreEncryptedLink(secretKeyHex, snapshotHex)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(handle))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun restoreEncryptedLinkHandshake(secretKeyHex: String, snapshotHex: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val handle = paykitRestoreEncryptedLinkHandshake(secretKeyHex, snapshotHex)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(handle))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun closeEncryptedLink(linkId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitCloseEncryptedLink(linkId)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun dropEncryptedLinkHandshake(handshakeId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitDropEncryptedLinkHandshake(handshakeId)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(""))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    companion object {
        const val NAME = "Paykit"
    }
}
