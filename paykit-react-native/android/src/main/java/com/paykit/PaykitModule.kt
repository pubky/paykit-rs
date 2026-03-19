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
                val jsonArray = JSONArray().apply {
                    entries.forEach { entry ->
                        put(JSONObject().apply {
                            put("method_id", entry.methodId)
                            put("endpoint_data", entry.endpointData)
                        })
                    }
                }.toString()
                withContext(Dispatchers.Main) {
                    promise.resolve(resultArray(jsonArray))
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

    companion object {
        const val NAME = "Paykit"
    }
}
