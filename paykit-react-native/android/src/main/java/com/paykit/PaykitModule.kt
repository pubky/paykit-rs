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

    private fun optionalResultArray(value: String?) = Arguments.createArray().apply {
        pushString("ok")
        if (value == null) {
            pushNull()
        } else {
            pushString(value)
        }
    }

    private fun errorArray(message: String) = Arguments.createArray().apply {
        pushString("error")
        pushString(message)
    }

    private fun paymentEndpointsFromJsonArray(array: JSONArray): List<FfiPaymentEndpoint> {
        return List(array.length()) { index ->
            val item = array.getJSONObject(index)
            requireAllowedKeys(
                item,
                "Payment Endpoint",
                setOf("payment_endpoint_identifier", "payment_endpoint_payload")
            )
            FfiPaymentEndpoint(
                paymentEndpointIdentifier = requiredString(item, "payment_endpoint_identifier"),
                paymentEndpointPayload = requiredString(item, "payment_endpoint_payload")
            )
        }
    }

    private fun paymentEndpointsJsonArray(paymentEndpoints: List<FfiPaymentEndpoint>): JSONArray {
        return JSONArray().apply {
            paymentEndpoints.forEach { paymentEndpoint ->
                put(JSONObject().apply {
                    put("payment_endpoint_identifier", paymentEndpoint.paymentEndpointIdentifier)
                    put("payment_endpoint_payload", paymentEndpoint.paymentEndpointPayload)
                })
            }
        }
    }

    private fun paymentEndpointsJson(paymentEndpoints: List<FfiPaymentEndpoint>): String {
        return paymentEndpointsJsonArray(paymentEndpoints).toString()
    }

    private fun requireAllowedKeys(objectValue: JSONObject, label: String, allowedKeys: Set<String>) {
        val keys = objectValue.keys()
        while (keys.hasNext()) {
            val key = keys.next()
            require(allowedKeys.contains(key)) {
                "$label contains unsupported field '$key'"
            }
        }
    }

    private fun requiredString(objectValue: JSONObject, key: String): String {
        val value = objectValue.get(key)
        require(value is String) {
            "$key must be a string"
        }
        return value
    }

    private fun requiredStringArray(objectValue: JSONObject, key: String): List<String> {
        val value = objectValue.get(key)
        require(value is JSONArray) {
            "$key must be an array of strings"
        }
        return List(value.length()) { index ->
            val item = value.get(index)
            require(item is String) {
                "$key must be an array of strings"
            }
            item
        }
    }

    private fun privatePaymentListFromJson(json: String): FfiPrivatePaymentList {
        val list = JSONObject(json)
        requireAllowedKeys(list, "Private Payment List", setOf("payment_endpoints"))
        return FfiPrivatePaymentList(
            paymentEndpoints = paymentEndpointsFromJsonArray(list.getJSONArray("payment_endpoints"))
        )
    }

    private fun privateApplicationMessagesJson(messages: List<FfiPrivateApplicationMessage>): String {
        return JSONArray().apply {
            messages.forEach { message ->
                put(JSONObject().apply {
                    put("version", message.version?.toLong() ?: JSONObject.NULL)
                    put("kind", message.kind ?: JSONObject.NULL)
                    put("raw_json", message.rawJson)
                })
            }
        }.toString()
    }

    private fun receiptJson(receipt: FfiReceipt): String {
        return JSONObject().apply {
            put("receipt_id", receipt.receiptId)
            put("payment_reference", receipt.paymentReference)
            put("payment_request_id", receipt.paymentRequestId ?: JSONObject.NULL)
            put("billing_period", billingPeriodJson(receipt.billingPeriod))
            put("recipient_public_key", receipt.recipientPublicKey)
            put("payment_endpoint_identifier", receipt.paymentEndpointIdentifier ?: JSONObject.NULL)
            put("amount", paymentAmountJson(receipt.amount))
            put("metadata", JSONObject(receipt.metadataJson))
        }.toString()
    }

    private fun nullableString(objectValue: JSONObject, key: String): String? {
        if (!objectValue.has(key) || objectValue.isNull(key)) {
            return null
        }
        val value = objectValue.get(key)
        require(value is String) {
            "$key must be a string or null"
        }
        return value
    }

    private fun metadataJsonFromObject(objectValue: JSONObject, key: String): String {
        if (!objectValue.has(key) || objectValue.isNull(key)) {
            return "{}"
        }
        val value = objectValue.get(key)
        require(value is JSONObject) {
            "$key must be a JSON object or null"
        }
        return value.toString()
    }

    private fun paymentAmountJson(amount: FfiPaymentAmount?): Any {
        return amount?.let {
            JSONObject().apply {
                put("value", it.value)
                put("asset", it.asset)
            }
        } ?: JSONObject.NULL
    }

    private fun paymentAmountFromJson(objectValue: JSONObject, key: String): FfiPaymentAmount? {
        if (!objectValue.has(key) || objectValue.isNull(key)) {
            return null
        }
        val value = objectValue.get(key)
        require(value is JSONObject) {
            "$key must be a JSON object or null"
        }
        requireAllowedKeys(value, "Payment Amount", setOf("value", "asset"))
        return FfiPaymentAmount(
            value = requiredString(value, "value"),
            asset = requiredString(value, "asset")
        )
    }

    private fun billingPeriodJson(period: FfiBillingPeriod?): Any {
        return period?.let {
            JSONObject().apply {
                put("starts_at", it.startsAt)
                put("ends_at", it.endsAt)
            }
        } ?: JSONObject.NULL
    }

    private fun billingPeriodFromJson(objectValue: JSONObject, key: String): FfiBillingPeriod? {
        if (!objectValue.has(key) || objectValue.isNull(key)) {
            return null
        }
        val value = objectValue.get(key)
        require(value is JSONObject) {
            "$key must be a JSON object or null"
        }
        requireAllowedKeys(value, "Billing Period", setOf("starts_at", "ends_at"))
        return FfiBillingPeriod(
            startsAt = requiredString(value, "starts_at"),
            endsAt = requiredString(value, "ends_at")
        )
    }

    private fun privateApplicationMessageFromJson(json: String): FfiPrivateApplicationMessage {
        val message = JSONObject(json)
        requireAllowedKeys(message, "Private Application Message", setOf("version", "kind", "raw_json"))
        val version = if (!message.has("version") || message.isNull("version")) {
            null
        } else {
            val value = message.get("version")
            require(value is Number) {
                "version must be a number or null"
            }
            uint32(value.toDouble(), "version")
        }
        return FfiPrivateApplicationMessage(
            version = version,
            kind = nullableString(message, "kind"),
            rawJson = requiredString(message, "raw_json")
        )
    }

    private fun recurrenceJson(recurrence: FfiRecurrence?): Any {
        return recurrence?.let {
            JSONObject().apply {
                put("every", it.every.toLong())
                put("unit", it.unit)
                put("starts_at", it.startsAt)
                put("anchor", it.anchor)
                put("ends_at", it.endsAt ?: JSONObject.NULL)
            }
        } ?: JSONObject.NULL
    }

    private fun recurrenceFromJson(objectValue: JSONObject, key: String): FfiRecurrence? {
        if (!objectValue.has(key) || objectValue.isNull(key)) {
            return null
        }
        val value = objectValue.get(key)
        require(value is JSONObject) {
            "$key must be a JSON object or null"
        }
        requireAllowedKeys(value, "Recurrence", setOf("every", "unit", "starts_at", "anchor", "ends_at"))
        val every = value.get("every")
        require(every is Number) {
            "every must be a number"
        }
        return FfiRecurrence(
            every = uint32(every.toDouble(), "every"),
            unit = requiredString(value, "unit"),
            startsAt = requiredString(value, "starts_at"),
            anchor = requiredString(value, "anchor"),
            endsAt = nullableString(value, "ends_at")
        )
    }

    private fun paymentRequestTermsJson(terms: FfiPaymentRequestTerms): JSONObject {
        return JSONObject().apply {
            put("amount", paymentAmountJson(terms.amount))
            put("payment_reference", terms.paymentReference)
            put("proposal_expires_at", terms.proposalExpiresAt ?: JSONObject.NULL)
            put("recurrence", recurrenceJson(terms.recurrence))
            put("accepted_payment_endpoint_identifiers", JSONArray(terms.acceptedPaymentEndpointIdentifiers))
            put("metadata", JSONObject(terms.metadataJson))
        }
    }

    private fun paymentRequestTermsFromJson(objectValue: JSONObject): FfiPaymentRequestTerms {
        requireAllowedKeys(
            objectValue,
            "Payment Request terms",
            setOf(
                "amount",
                "payment_reference",
                "proposal_expires_at",
                "recurrence",
                "accepted_payment_endpoint_identifiers",
                "metadata"
            )
        )
        return FfiPaymentRequestTerms(
            amount = paymentAmountFromJson(objectValue, "amount")
                ?: error("amount is required"),
            paymentReference = requiredString(objectValue, "payment_reference"),
            proposalExpiresAt = nullableString(objectValue, "proposal_expires_at"),
            recurrence = recurrenceFromJson(objectValue, "recurrence"),
            acceptedPaymentEndpointIdentifiers = requiredStringArray(
                objectValue,
                "accepted_payment_endpoint_identifiers"
            ),
            metadataJson = metadataJsonFromObject(objectValue, "metadata")
        )
    }

    private fun paymentRequestJson(event: FfiPaymentRequest): JSONObject {
        return JSONObject().apply {
            put("event_id", event.eventId)
            put("payment_request_id", event.paymentRequestId)
            put("request", paymentRequestTermsJson(event.request))
        }
    }

    private fun paymentRequestFromJson(objectValue: JSONObject): FfiPaymentRequest {
        requireAllowedKeys(objectValue, "Payment Request", setOf("event_id", "payment_request_id", "request"))
        return FfiPaymentRequest(
            eventId = requiredString(objectValue, "event_id"),
            paymentRequestId = requiredString(objectValue, "payment_request_id"),
            request = paymentRequestTermsFromJson(objectValue.getJSONObject("request"))
        )
    }

    private fun paymentRequestAcceptanceJson(event: FfiPaymentRequestAcceptance): JSONObject {
        return JSONObject().apply {
            put("event_id", event.eventId)
            put("payment_request_id", event.paymentRequestId)
        }
    }

    private fun paymentRequestAcceptanceFromJson(objectValue: JSONObject): FfiPaymentRequestAcceptance {
        requireAllowedKeys(objectValue, "Payment Request Acceptance", setOf("event_id", "payment_request_id"))
        return FfiPaymentRequestAcceptance(
            eventId = requiredString(objectValue, "event_id"),
            paymentRequestId = requiredString(objectValue, "payment_request_id")
        )
    }

    private fun paymentRequestRejectionJson(event: FfiPaymentRequestRejection): JSONObject {
        return JSONObject().apply {
            put("event_id", event.eventId)
            put("payment_request_id", event.paymentRequestId)
            put("reason", event.reason ?: JSONObject.NULL)
        }
    }

    private fun paymentRequestRejectionFromJson(objectValue: JSONObject): FfiPaymentRequestRejection {
        requireAllowedKeys(objectValue, "Payment Request Rejection", setOf("event_id", "payment_request_id", "reason"))
        return FfiPaymentRequestRejection(
            eventId = requiredString(objectValue, "event_id"),
            paymentRequestId = requiredString(objectValue, "payment_request_id"),
            reason = nullableString(objectValue, "reason")
        )
    }

    private fun paymentRequestCancellationJson(event: FfiPaymentRequestCancellation): JSONObject {
        return JSONObject().apply {
            put("event_id", event.eventId)
            put("payment_request_id", event.paymentRequestId)
            put("reason", event.reason ?: JSONObject.NULL)
        }
    }

    private fun paymentRequestCancellationFromJson(objectValue: JSONObject): FfiPaymentRequestCancellation {
        requireAllowedKeys(objectValue, "Payment Request Cancellation", setOf("event_id", "payment_request_id", "reason"))
        return FfiPaymentRequestCancellation(
            eventId = requiredString(objectValue, "event_id"),
            paymentRequestId = requiredString(objectValue, "payment_request_id"),
            reason = nullableString(objectValue, "reason")
        )
    }

    private fun paymentProofJson(proof: FfiPaymentProof): JSONObject {
        return JSONObject().apply {
            put("event_id", proof.eventId)
            put("payment_request_id", proof.paymentRequestId)
            put("payment_reference", proof.paymentReference)
            put("billing_period", billingPeriodJson(proof.billingPeriod))
            put("payment_endpoint_identifier", proof.paymentEndpointIdentifier)
            put("proof", JSONObject(proof.proofJson))
        }
    }

    private fun paymentProofFromJson(objectValue: JSONObject): FfiPaymentProof {
        requireAllowedKeys(
            objectValue,
            "Payment Proof",
            setOf(
                "event_id",
                "payment_request_id",
                "payment_reference",
                "billing_period",
                "payment_endpoint_identifier",
                "proof"
            )
        )
        val proof = objectValue.get("proof")
        require(proof is JSONObject) {
            "proof must be a JSON object"
        }
        return FfiPaymentProof(
            eventId = requiredString(objectValue, "event_id"),
            paymentRequestId = requiredString(objectValue, "payment_request_id"),
            paymentReference = requiredString(objectValue, "payment_reference"),
            billingPeriod = billingPeriodFromJson(objectValue, "billing_period"),
            paymentEndpointIdentifier = requiredString(objectValue, "payment_endpoint_identifier"),
            proofJson = proof.toString()
        )
    }

    private fun paymentRequestEventJson(event: FfiPaymentRequestEvent): JSONObject {
        return JSONObject().apply {
            put("event_type", event.eventType)
            put("request", event.request?.let { paymentRequestJson(it) } ?: JSONObject.NULL)
            put("acceptance", event.acceptance?.let { paymentRequestAcceptanceJson(it) } ?: JSONObject.NULL)
            put("rejection", event.rejection?.let { paymentRequestRejectionJson(it) } ?: JSONObject.NULL)
            put("cancellation", event.cancellation?.let { paymentRequestCancellationJson(it) } ?: JSONObject.NULL)
            put("proof", event.proof?.let { paymentProofJson(it) } ?: JSONObject.NULL)
        }
    }

    private fun paymentRequestEventFromJson(objectValue: JSONObject): FfiPaymentRequestEvent {
        requireAllowedKeys(
            objectValue,
            "Payment Request Event",
            setOf("event_type", "request", "acceptance", "rejection", "cancellation", "proof")
        )
        return FfiPaymentRequestEvent(
            eventType = requiredString(objectValue, "event_type"),
            request = if (objectValue.has("request") && !objectValue.isNull("request")) {
                paymentRequestFromJson(objectValue.getJSONObject("request"))
            } else {
                null
            },
            acceptance = if (objectValue.has("acceptance") && !objectValue.isNull("acceptance")) {
                paymentRequestAcceptanceFromJson(objectValue.getJSONObject("acceptance"))
            } else {
                null
            },
            rejection = if (objectValue.has("rejection") && !objectValue.isNull("rejection")) {
                paymentRequestRejectionFromJson(objectValue.getJSONObject("rejection"))
            } else {
                null
            },
            cancellation = if (objectValue.has("cancellation") && !objectValue.isNull("cancellation")) {
                paymentRequestCancellationFromJson(objectValue.getJSONObject("cancellation"))
            } else {
                null
            },
            proof = if (objectValue.has("proof") && !objectValue.isNull("proof")) {
                paymentProofFromJson(objectValue.getJSONObject("proof"))
            } else {
                null
            }
        )
    }

    private fun paymentRequestEventMessageJson(message: FfiPaymentRequestEventMessage?): String {
        return message?.let {
            JSONObject().apply {
                put("kind", it.kind)
                put("event_id", it.eventId ?: JSONObject.NULL)
                put("payment_request_id", it.paymentRequestId ?: JSONObject.NULL)
                put("raw_json", it.rawJson)
                put("event", it.event?.let { event -> paymentRequestEventJson(event) } ?: JSONObject.NULL)
                put("validation_error", it.validationError ?: JSONObject.NULL)
            }.toString()
        } ?: "null"
    }

    private fun receiptAccessEventMessageJson(message: FfiReceiptAccessEventMessage?): String {
        return message?.let {
            JSONObject().apply {
                put("kind", it.kind)
                put("event_id", it.eventId ?: JSONObject.NULL)
                put("receipt_id", it.receiptId ?: JSONObject.NULL)
                put("raw_json", it.rawJson)
                put("access", it.access?.let { access -> receiptAccessJson(access) } ?: JSONObject.NULL)
                put("validation_error", it.validationError ?: JSONObject.NULL)
            }.toString()
        } ?: "null"
    }

    private fun receiptDraftFromJson(json: String): FfiReceiptDraft {
        val draft = JSONObject(json)
        requireAllowedKeys(
            draft,
            "Receipt Draft",
            setOf(
                "receipt_id",
                "payment_reference",
                "payment_request_id",
                "billing_period",
                "payment_endpoint_identifier",
                "amount",
                "metadata"
            )
        )
        return FfiReceiptDraft(
            receiptId = nullableString(draft, "receipt_id"),
            paymentReference = requiredString(draft, "payment_reference"),
            paymentRequestId = nullableString(draft, "payment_request_id"),
            billingPeriod = billingPeriodFromJson(draft, "billing_period"),
            paymentEndpointIdentifier = nullableString(draft, "payment_endpoint_identifier"),
            amount = paymentAmountFromJson(draft, "amount"),
            metadataJson = metadataJsonFromObject(draft, "metadata")
        )
    }

    private fun receiptFromJson(objectValue: JSONObject): FfiReceipt {
        requireAllowedKeys(
            objectValue,
            "Receipt",
            setOf(
                "receipt_id",
                "payment_reference",
                "payment_request_id",
                "billing_period",
                "recipient_public_key",
                "payment_endpoint_identifier",
                "amount",
                "metadata"
            )
        )
        return FfiReceipt(
            receiptId = requiredString(objectValue, "receipt_id"),
            paymentReference = requiredString(objectValue, "payment_reference"),
            paymentRequestId = nullableString(objectValue, "payment_request_id"),
            billingPeriod = billingPeriodFromJson(objectValue, "billing_period"),
            recipientPublicKey = requiredString(objectValue, "recipient_public_key"),
            paymentEndpointIdentifier = nullableString(objectValue, "payment_endpoint_identifier"),
            amount = paymentAmountFromJson(objectValue, "amount"),
            metadataJson = metadataJsonFromObject(objectValue, "metadata")
        )
    }

    private fun receiptAccessJson(access: FfiReceiptAccess): JSONObject {
        return JSONObject().apply {
            put("event_id", access.eventId)
            put("receipt_id", access.receiptId)
            put("payment_reference", access.paymentReference)
            put("payment_request_id", access.paymentRequestId ?: JSONObject.NULL)
            put("billing_period", billingPeriodJson(access.billingPeriod))
            put("location", access.location)
            put("key", access.key)
        }
    }

    private fun receiptAccessFromJson(objectValue: JSONObject): FfiReceiptAccess {
        requireAllowedKeys(
            objectValue,
            "Receipt Access",
            setOf(
                "event_id",
                "receipt_id",
                "payment_reference",
                "payment_request_id",
                "billing_period",
                "location",
                "key"
            )
        )
        return FfiReceiptAccess(
            eventId = requiredString(objectValue, "event_id"),
            receiptId = requiredString(objectValue, "receipt_id"),
            paymentReference = requiredString(objectValue, "payment_reference"),
            paymentRequestId = nullableString(objectValue, "payment_request_id"),
            billingPeriod = billingPeriodFromJson(objectValue, "billing_period"),
            location = requiredString(objectValue, "location"),
            key = requiredString(objectValue, "key")
        )
    }

    private fun preparedReceiptJson(prepared: FfiPreparedReceipt): String {
        return JSONObject().apply {
            put("receipt", JSONObject(receiptJson(prepared.receipt)))
            put("encrypted_receipt", prepared.encryptedReceipt)
            put("access", receiptAccessJson(prepared.access))
        }.toString()
    }

    private fun preparedReceiptFromJson(json: String): FfiPreparedReceipt {
        val prepared = JSONObject(json)
        requireAllowedKeys(
            prepared,
            "Prepared Receipt",
            setOf("receipt", "encrypted_receipt", "access")
        )
        return FfiPreparedReceipt(
            receipt = receiptFromJson(prepared.getJSONObject("receipt")),
            encryptedReceipt = requiredString(prepared, "encrypted_receipt"),
            access = receiptAccessFromJson(prepared.getJSONObject("access"))
        )
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
                val paymentEndpoints = paykitGetPaymentList(publicKey)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(paymentEndpointsJson(paymentEndpoints)))
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
                    promise.resolve(optionalResultArray(result))
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
    fun setPrivatePaymentList(linkId: String, payloadJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSetPrivatePaymentList(linkId, privatePaymentListFromJson(payloadJson))
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
    fun receivePrivateApplicationMessages(linkId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val messages = paykitReceivePrivateApplicationMessages(linkId)
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(privateApplicationMessagesJson(messages)))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun parsePrivatePaymentListJson(json: String, promise: Promise) {
        try {
            val list = paykitParsePrivatePaymentListJson(json)
            promise.resolve(resultArray(JSONObject().apply {
                put("payment_endpoints", paymentEndpointsJsonArray(list.paymentEndpoints))
            }.toString()))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun parsePaymentRequestEventMessage(messageJson: String, promise: Promise) {
        try {
            val message = privateApplicationMessageFromJson(messageJson)
            val eventMessage = paykitParsePaymentRequestEventMessage(message)
            promise.resolve(resultArray(paymentRequestEventMessageJson(eventMessage)))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun serializePaymentRequestEvent(eventJson: String, promise: Promise) {
        try {
            val event = paymentRequestEventFromJson(JSONObject(eventJson))
            promise.resolve(resultArray(paykitSerializePaymentRequestEvent(event)))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun validatePaymentProofForRequest(proofJson: String, requestJson: String, promise: Promise) {
        try {
            paykitValidatePaymentProofForRequest(
                paymentProofFromJson(JSONObject(proofJson)),
                paymentRequestFromJson(JSONObject(requestJson))
            )
            promise.resolve(resultArray(""))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun sendPaymentRequest(linkId: String, eventJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSendPaymentRequest(linkId, paymentRequestFromJson(JSONObject(eventJson)))
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
    fun sendPaymentRequestAcceptance(linkId: String, eventJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSendPaymentRequestAcceptance(
                    linkId,
                    paymentRequestAcceptanceFromJson(JSONObject(eventJson))
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
    fun sendPaymentRequestRejection(linkId: String, eventJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSendPaymentRequestRejection(
                    linkId,
                    paymentRequestRejectionFromJson(JSONObject(eventJson))
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
    fun sendPaymentRequestCancellation(linkId: String, eventJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSendPaymentRequestCancellation(
                    linkId,
                    paymentRequestCancellationFromJson(JSONObject(eventJson))
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
    fun sendPaymentProof(linkId: String, eventJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSendPaymentProof(linkId, paymentProofFromJson(JSONObject(eventJson)))
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
    fun prepareReceipt(linkId: String, draftJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val prepared = paykitPrepareReceipt(linkId, receiptDraftFromJson(draftJson))
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(preparedReceiptJson(prepared)))
                }
            } catch (e: Exception) {
                withContext(Dispatchers.Main) {
                    promise.resolve(errorArray(e.message ?: "Unknown error"))
                }
            }
        }
    }

    @ReactMethod
    fun parseReceiptAccessEventMessage(messageJson: String, promise: Promise) {
        try {
            val message = privateApplicationMessageFromJson(messageJson)
            val eventMessage = paykitParseReceiptAccessEventMessage(message)
            promise.resolve(resultArray(receiptAccessEventMessageJson(eventMessage)))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun parseReceiptAccessJson(json: String, promise: Promise) {
        try {
            val access = paykitParseReceiptAccessJson(json)
            promise.resolve(resultArray(receiptAccessJson(access).toString()))
        } catch (e: Exception) {
            promise.resolve(errorArray(e.message ?: "Unknown error"))
        }
    }

    @ReactMethod
    fun storePreparedReceipt(preparedJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitStorePreparedReceipt(preparedReceiptFromJson(preparedJson))
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
    fun sendReceiptAccess(linkId: String, accessJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSendReceiptAccess(linkId, receiptAccessFromJson(JSONObject(accessJson)))
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
    fun receiptLocation(receiptId: String, promise: Promise) {
        try {
            promise.resolve(resultArray(paykitReceiptLocation(receiptId)))
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
