package com.paykit

import com.facebook.react.bridge.Arguments
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactContextBaseJavaModule
import com.facebook.react.bridge.ReactMethod
import com.facebook.react.bridge.Promise
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject
import com.synonym.paykit.*

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

    private fun entriesFromJson(json: String): List<FfiPaymentEntry> {
        val array = JSONArray(json)
        return List(array.length()) { index ->
            val item = array.getJSONObject(index)
            FfiPaymentEntry(
                methodId = item.getString("method_id"),
                endpointData = item.getString("endpoint_data")
            )
        }
    }

    private fun entriesJson(entries: List<FfiPaymentEntry>): String {
        return JSONArray().apply {
            entries.forEach { entry ->
                put(JSONObject().apply {
                    put("method_id", entry.methodId)
                    put("endpoint_data", entry.endpointData)
                })
            }
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
    // Payment list (read)
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
    fun getPaymentEndpoint(publicKey: String, methodId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val result = paykitGetPaymentEndpoint(publicKey, methodId)
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
    fun setPaymentEndpoint(methodId: String, endpointData: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSetPaymentEndpoint(methodId, endpointData)
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
    fun removePaymentEndpoint(methodId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitRemovePaymentEndpoint(methodId)
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
    fun setPrivatePayments(linkId: String, entriesJson: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                paykitSetPrivatePayments(linkId, entriesFromJson(entriesJson))
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
    fun getPrivatePayments(linkId: String, promise: Promise) {
        CoroutineScope(Dispatchers.IO).launch {
            try {
                val entries = paykitGetPrivatePayments(linkId)
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
