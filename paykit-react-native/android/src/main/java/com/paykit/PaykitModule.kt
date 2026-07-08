package com.paykit

import android.util.Base64
import com.facebook.react.bridge.Arguments
import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactContextBaseJavaModule
import com.facebook.react.bridge.ReactMethod
import com.synonym.paykit.FfiEncryptedLinkRecoveryMarkerPolicy
import com.synonym.paykit.FfiEndpointManagementScope
import com.synonym.paykit.FfiPaykitSdkConfig
import com.synonym.paykit.PaykitFfiException
import com.synonym.paykit.FfiPublicContactSharingPolicy
import com.synonym.paykit.FfiPubkyAuthRequestKind
import com.synonym.paykit.FfiPubkyClientConfig
import com.synonym.paykit.PaykitAndroid
import com.synonym.paykit.coreSessionCapabilities
import com.synonym.paykit.defaultConfig
import com.synonym.paykit.defaultPubkyClientConfig
import com.synonym.paykit.parsePubkyAuthUrl
import com.synonym.paykit.parsePubkyResource
import com.synonym.paykit.pubkyPublicKeyFromSecret
import com.synonym.paykit.pubkySecretKeyFromBip39Mnemonic
import com.synonym.paykit.pubkySecretKeyFromBip39Seed
import com.synonym.paykit.requiredSessionCapabilities
import com.synonym.paykit.resolvePubkyUrl
import org.json.JSONObject

class PaykitModule(private val reactContext: ReactApplicationContext) :
    ReactContextBaseJavaModule(reactContext) {

    private val nativeInitialized = PaykitAndroid.initialize(reactContext)

    override fun getName(): String = NAME

    private fun resultArray(value: String) = Arguments.createArray().apply {
        pushString("ok")
        pushString(value)
    }

    private fun errorArray(category: String, code: String, context: String) = Arguments.createArray().apply {
        pushString("error")
        pushString(
            JSONObject()
                .put("category", category)
                .put("code", code)
                .put("context", context)
                .toString()
        )
    }

    private fun errorArray(error: PaykitFfiException) = when (error) {
        is PaykitFfiException.Storage -> errorArray("storage", error.code, error.context)
        is PaykitFfiException.Identity -> errorArray("identity", error.code, error.context)
        is PaykitFfiException.Transport -> errorArray("transport", error.code, error.context)
        is PaykitFfiException.NotFound -> errorArray("not_found", error.code, error.context)
        is PaykitFfiException.Protocol -> errorArray("protocol", error.code, error.context)
        is PaykitFfiException.Policy -> errorArray("policy", error.code, error.context)
        is PaykitFfiException.PaymentAdapter -> errorArray(
            "payment_adapter",
            error.code,
            error.context
        )
        is PaykitFfiException.RecoveryRequired -> errorArray(
            "recovery_required",
            error.code,
            error.context
        )
    }

    private fun resolveResult(promise: Promise, block: () -> String) {
        if (!nativeInitialized) {
            promise.resolve(
                errorArray(
                    "platform",
                    "android_initialization_failed",
                    "Android TLS verifier initialization failed"
                )
            )
            return
        }

        try {
            promise.resolve(resultArray(block()))
        } catch (error: PaykitFfiException) {
            promise.resolve(errorArray(error))
        } catch (error: IllegalArgumentException) {
            promise.resolve(
                errorArray("protocol", "validation", "invalid React Native bridge input")
            )
        } catch (error: org.json.JSONException) {
            promise.resolve(
                errorArray("protocol", "validation", "invalid React Native bridge input")
            )
        } catch (error: Throwable) {
            promise.resolve(
                errorArray("platform", "platform_error", "native platform call failed")
            )
        }
    }

    private fun bytesFromBase64(value: String, label: String): ByteArray {
        return try {
            Base64.decode(value, Base64.NO_WRAP)
        } catch (error: IllegalArgumentException) {
            throw IllegalArgumentException("$label must be base64")
        }
    }

    private fun configFromJson(json: String): FfiPaykitSdkConfig {
        val value = JSONObject(json)
        return FfiPaykitSdkConfig(
            profileNamespace = value.getString("profile_namespace"),
            endpointManagementScope = endpointManagementScope(
                value.getString("endpoint_management_scope")
            ),
            encryptedLinkRecoveryMarkers = recoveryMarkerPolicy(
                value.getString("encrypted_link_recovery_markers")
            ),
            publicContactSharing = publicContactSharingPolicy(
                value.getString("public_contact_sharing")
            ),
            peerLinkOperationLeaseTimeoutSecs = unsignedLong(
                value,
                "peer_link_operation_lease_timeout_secs"
            ),
            outboundPrivateSendLeaseTimeoutSecs = unsignedLong(
                value,
                "outbound_private_send_lease_timeout_secs"
            ),
            outboundPrivateRetryBackoffSecs = unsignedLong(
                value,
                "outbound_private_retry_backoff_secs"
            )
        )
    }

    private fun unsignedLong(value: JSONObject, key: String): ULong {
        val number = value.getLong(key)
        require(number >= 0) {
            "$key must be a non-negative integer"
        }
        return number.toULong()
    }

    private fun configJson(config: FfiPaykitSdkConfig): String {
        return JSONObject()
            .put("profile_namespace", config.profileNamespace)
            .put(
                "endpoint_management_scope",
                endpointManagementScopeString(config.endpointManagementScope)
            )
            .put(
                "encrypted_link_recovery_markers",
                recoveryMarkerPolicyString(config.encryptedLinkRecoveryMarkers)
            )
            .put(
                "public_contact_sharing",
                publicContactSharingPolicyString(config.publicContactSharing)
            )
            .put(
                "peer_link_operation_lease_timeout_secs",
                config.peerLinkOperationLeaseTimeoutSecs.toLong()
            )
            .put(
                "outbound_private_send_lease_timeout_secs",
                config.outboundPrivateSendLeaseTimeoutSecs.toLong()
            )
            .put(
                "outbound_private_retry_backoff_secs",
                config.outboundPrivateRetryBackoffSecs.toLong()
            )
            .toString()
    }

    private fun pubkyClientConfigJson(config: FfiPubkyClientConfig): String {
        return JSONObject()
            .put("request_timeout_secs", config.requestTimeoutSecs.toLong())
            .toString()
    }

    private fun endpointManagementScope(value: String): FfiEndpointManagementScope {
        return when (value) {
            "managed_only" -> FfiEndpointManagementScope.MANAGED_ONLY
            "full_paykit_namespace" -> FfiEndpointManagementScope.FULL_PAYKIT_NAMESPACE
            else -> throw IllegalArgumentException("unsupported endpoint_management_scope value '$value'")
        }
    }

    private fun endpointManagementScopeString(value: FfiEndpointManagementScope): String {
        return when (value) {
            FfiEndpointManagementScope.MANAGED_ONLY -> "managed_only"
            FfiEndpointManagementScope.FULL_PAYKIT_NAMESPACE -> "full_paykit_namespace"
            FfiEndpointManagementScope.UNKNOWN -> "unknown"
        }
    }

    private fun recoveryMarkerPolicy(value: String): FfiEncryptedLinkRecoveryMarkerPolicy {
        return when (value) {
            "enabled" -> FfiEncryptedLinkRecoveryMarkerPolicy.ENABLED
            "disabled" -> FfiEncryptedLinkRecoveryMarkerPolicy.DISABLED
            else -> throw IllegalArgumentException("unsupported encrypted_link_recovery_markers value '$value'")
        }
    }

    private fun recoveryMarkerPolicyString(value: FfiEncryptedLinkRecoveryMarkerPolicy): String {
        return when (value) {
            FfiEncryptedLinkRecoveryMarkerPolicy.ENABLED -> "enabled"
            FfiEncryptedLinkRecoveryMarkerPolicy.DISABLED -> "disabled"
            FfiEncryptedLinkRecoveryMarkerPolicy.UNKNOWN -> "unknown"
        }
    }

    private fun publicContactSharingPolicy(value: String): FfiPublicContactSharingPolicy {
        return when (value) {
            "local_only" -> FfiPublicContactSharingPolicy.LOCAL_ONLY
            "configured_public_namespace" -> FfiPublicContactSharingPolicy.CONFIGURED_PUBLIC_NAMESPACE
            else -> throw IllegalArgumentException("unsupported public_contact_sharing value '$value'")
        }
    }

    private fun publicContactSharingPolicyString(value: FfiPublicContactSharingPolicy): String {
        return when (value) {
            FfiPublicContactSharingPolicy.LOCAL_ONLY -> "local_only"
            FfiPublicContactSharingPolicy.CONFIGURED_PUBLIC_NAMESPACE -> "configured_public_namespace"
            FfiPublicContactSharingPolicy.UNKNOWN -> "unknown"
        }
    }

    private fun authRequestKindString(value: FfiPubkyAuthRequestKind): String {
        return when (value) {
            FfiPubkyAuthRequestKind.SIGN_IN -> "sign_in"
            FfiPubkyAuthRequestKind.SIGN_UP -> "sign_up"
            FfiPubkyAuthRequestKind.SECRET_EXPORT -> "secret_export"
            FfiPubkyAuthRequestKind.UNKNOWN -> "unknown"
        }
    }

    @ReactMethod
    fun sdkDefaultConfig(promise: Promise) {
        resolveResult(promise) {
            configJson(defaultConfig())
        }
    }

    @ReactMethod
    fun sdkDefaultPubkyClientConfig(promise: Promise) {
        resolveResult(promise) {
            pubkyClientConfigJson(defaultPubkyClientConfig())
        }
    }

    @ReactMethod
    fun sdkRequiredSessionCapabilities(configJson: String, promise: Promise) {
        resolveResult(promise) {
            requiredSessionCapabilities(configFromJson(configJson))
        }
    }

    @ReactMethod
    fun sdkCoreSessionCapabilities(promise: Promise) {
        resolveResult(promise) {
            coreSessionCapabilities()
        }
    }

    @ReactMethod
    fun sdkPubkyPublicKeyFromBip39Seed(seedBase64: String, promise: Promise) {
        resolveResult(promise) {
            val seed = bytesFromBase64(seedBase64, "seed")
            pubkyPublicKeyFromSecret(pubkySecretKeyFromBip39Seed(seed))
        }
    }

    @ReactMethod
    fun sdkPubkyPublicKeyFromBip39Mnemonic(mnemonicPhrase: String, promise: Promise) {
        resolveResult(promise) {
            pubkyPublicKeyFromSecret(pubkySecretKeyFromBip39Mnemonic(mnemonicPhrase))
        }
    }

    @ReactMethod
    fun sdkParsePubkyAuthUrl(authUrl: String, promise: Promise) {
        resolveResult(promise) {
            val details = parsePubkyAuthUrl(authUrl)
            JSONObject()
                .put("kind", authRequestKindString(details.kind))
                .put("capabilities", details.capabilities ?: JSONObject.NULL)
                .put("relay_url", details.relayUrl ?: JSONObject.NULL)
                .put("homeserver_public_key", details.homeserverPublicKey ?: JSONObject.NULL)
                .toString()
        }
    }

    @ReactMethod
    fun sdkResolvePubkyUrl(uri: String, promise: Promise) {
        resolveResult(promise) {
            resolvePubkyUrl(uri)
        }
    }

    @ReactMethod
    fun sdkParsePubkyResource(uri: String, promise: Promise) {
        resolveResult(promise) {
            val resource = parsePubkyResource(uri)
            JSONObject()
                .put("public_key", resource.publicKey)
                .put("path", resource.path)
                .put("transport_url", resource.transportUrl)
                .toString()
        }
    }

    companion object {
        const val NAME = "Paykit"
    }
}
