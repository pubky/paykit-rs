import Foundation
import React

@objc(Paykit)
class Paykit: RCTEventEmitter {
    @objc override static func requiresMainQueueSetup() -> Bool {
        return false
    }

    override func supportedEvents() -> [String]! {
        return []
    }

    private func resultArray(_ value: String) -> [String] {
        return ["ok", value]
    }

    private func errorArray(category: String, code: String, context: String) -> [String] {
        return ["error", errorJson(category: category, code: code, context: context)]
    }

    private func errorJson(category: String, code: String, context: String) -> String {
        (try? jsonString([
            "category": category,
            "code": code,
            "context": context
        ])) ?? "{\"category\":\"platform\",\"code\":\"error\",\"context\":\"failed to encode error\"}"
    }

    private func errorArray(_ error: PaykitFfiError) -> [String] {
        switch error {
        case let .Storage(code: code, context: context):
            return errorArray(category: "storage", code: code, context: context)
        case let .Identity(code: code, context: context):
            return errorArray(category: "identity", code: code, context: context)
        case let .Transport(code: code, context: context):
            return errorArray(category: "transport", code: code, context: context)
        case let .NotFound(code: code, context: context):
            return errorArray(category: "not_found", code: code, context: context)
        case let .Protocol(code: code, context: context):
            return errorArray(category: "protocol", code: code, context: context)
        case let .Policy(code: code, context: context):
            return errorArray(category: "policy", code: code, context: context)
        case let .PaymentAdapter(code: code, context: context):
            return errorArray(category: "payment_adapter", code: code, context: context)
        case let .RecoveryRequired(code: code, context: context):
            return errorArray(category: "recovery_required", code: code, context: context)
        }
    }

    private func resolveResult(
        _ resolve: RCTPromiseResolveBlock,
        _ block: () throws -> String
    ) {
        do {
            resolve(resultArray(try block()))
        } catch let error as PaykitFfiError {
            resolve(errorArray(error))
        } catch let error as NSError where error.domain == "Paykit" {
            resolve(errorArray(
                category: "protocol",
                code: "validation",
                context: "invalid React Native bridge input"
            ))
        } catch {
            resolve(errorArray(
                category: "platform",
                code: "platform_error",
                context: "native platform call failed"
            ))
        }
    }

    private func jsonObject(_ json: String, label: String) throws -> [String: Any] {
        let data = Data(json.utf8)
        guard let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(label) must be a JSON object"
            ])
        }
        return object
    }

    private func jsonString(_ object: Any) throws -> String {
        let data = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        return String(data: data, encoding: .utf8) ?? "{}"
    }

    private func requiredString(_ object: [String: Any], _ key: String) throws -> String {
        guard let value = object[key] as? String else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(key) must be a string"
            ])
        }
        return value
    }

    private func requiredUInt64(_ object: [String: Any], _ key: String) throws -> UInt64 {
        guard let number = object[key] as? NSNumber,
              CFGetTypeID(number) != CFBooleanGetTypeID(),
              number.doubleValue >= 0,
              number.doubleValue.rounded(.down) == number.doubleValue else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(key) must be a non-negative integer"
            ])
        }
        return number.uint64Value
    }

    private func dataFromBase64(_ value: String, label: String) throws -> Data {
        guard let data = Data(base64Encoded: value) else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(label) must be base64"
            ])
        }
        return data
    }

    private func configFromJson(_ json: String) throws -> FfiPaykitSdkConfig {
        let object = try jsonObject(json, label: "PaykitSdkConfig")
        return FfiPaykitSdkConfig(
            receiverId: try requiredString(object, "receiver_id"),
            profileNamespace: try requiredString(object, "profile_namespace"),
            endpointManagementScope: try endpointManagementScope(
                try requiredString(object, "endpoint_management_scope")
            ),
            encryptedLinkRecoveryMarkers: try recoveryMarkerPolicy(
                try requiredString(object, "encrypted_link_recovery_markers")
            ),
            publicContactSharing: try publicContactSharingPolicy(
                try requiredString(object, "public_contact_sharing")
            ),
            peerLinkOperationLeaseTimeoutSecs: try requiredUInt64(
                object,
                "peer_link_operation_lease_timeout_secs"
            ),
            outboundPrivateSendLeaseTimeoutSecs: try requiredUInt64(
                object,
                "outbound_private_send_lease_timeout_secs"
            ),
            outboundPrivateRetryBackoffSecs: try requiredUInt64(
                object,
                "outbound_private_retry_backoff_secs"
            )
        )
    }

    private func configJson(_ config: FfiPaykitSdkConfig) throws -> String {
        try jsonString([
            "receiver_id": config.receiverId,
            "profile_namespace": config.profileNamespace,
            "endpoint_management_scope": endpointManagementScopeString(config.endpointManagementScope),
            "encrypted_link_recovery_markers": recoveryMarkerPolicyString(config.encryptedLinkRecoveryMarkers),
            "public_contact_sharing": publicContactSharingPolicyString(config.publicContactSharing),
            "peer_link_operation_lease_timeout_secs": config.peerLinkOperationLeaseTimeoutSecs,
            "outbound_private_send_lease_timeout_secs": config.outboundPrivateSendLeaseTimeoutSecs,
            "outbound_private_retry_backoff_secs": config.outboundPrivateRetryBackoffSecs
        ])
    }

    private func pubkyClientConfigJson(_ config: FfiPubkyClientConfig) throws -> String {
        try jsonString([
            "request_timeout_secs": config.requestTimeoutSecs
        ])
    }

    private func authDetailsJson(_ details: FfiPubkyAuthDetails) throws -> String {
        try jsonString([
            "kind": authRequestKindString(details.kind),
            "capabilities": details.capabilities ?? NSNull(),
            "relay_url": details.relayUrl ?? NSNull(),
            "homeserver_public_key": details.homeserverPublicKey ?? NSNull()
        ])
    }

    private func resourceRefJson(_ resource: FfiPubkyResourceRef) throws -> String {
        try jsonString([
            "public_key": resource.publicKey,
            "path": resource.path,
            "transport_url": resource.transportUrl
        ])
    }

    private func endpointManagementScope(_ value: String) throws -> FfiEndpointManagementScope {
        switch value {
        case "managed_only": return .managedOnly
        case "full_paykit_namespace": return .fullPaykitNamespace
        default: throw enumError("endpoint_management_scope", value)
        }
    }

    private func endpointManagementScopeString(_ value: FfiEndpointManagementScope) -> String {
        switch value {
        case .managedOnly: return "managed_only"
        case .fullPaykitNamespace: return "full_paykit_namespace"
        case .unknown: return "unknown"
        }
    }

    private func recoveryMarkerPolicy(_ value: String) throws -> FfiEncryptedLinkRecoveryMarkerPolicy {
        switch value {
        case "enabled": return .enabled
        case "disabled": return .disabled
        default: throw enumError("encrypted_link_recovery_markers", value)
        }
    }

    private func recoveryMarkerPolicyString(_ value: FfiEncryptedLinkRecoveryMarkerPolicy) -> String {
        switch value {
        case .enabled: return "enabled"
        case .disabled: return "disabled"
        case .unknown: return "unknown"
        }
    }

    private func publicContactSharingPolicy(_ value: String) throws -> FfiPublicContactSharingPolicy {
        switch value {
        case "local_only": return .localOnly
        case "configured_public_namespace": return .configuredPublicNamespace
        default: throw enumError("public_contact_sharing", value)
        }
    }

    private func publicContactSharingPolicyString(_ value: FfiPublicContactSharingPolicy) -> String {
        switch value {
        case .localOnly: return "local_only"
        case .configuredPublicNamespace: return "configured_public_namespace"
        case .unknown: return "unknown"
        }
    }

    private func authRequestKindString(_ value: FfiPubkyAuthRequestKind) -> String {
        switch value {
        case .signIn: return "sign_in"
        case .signUp: return "sign_up"
        case .secretExport: return "secret_export"
        case .unknown: return "unknown"
        }
    }

    private func enumError(_ field: String, _ value: String) -> NSError {
        NSError(domain: "Paykit", code: 1, userInfo: [
            NSLocalizedDescriptionKey: "unsupported \(field) value '\(value)'"
        ])
    }

    @objc(sdkDefaultConfig:withResolver:withRejecter:)
    func sdkDefaultConfig(
        receiverId: String,
        resolve: RCTPromiseResolveBlock,
        reject: RCTPromiseRejectBlock
    ) {
        resolveResult(resolve) {
            try configJson(defaultConfig(receiverId: receiverId))
        }
    }

    @objc(sdkDefaultPubkyClientConfig:rejecter:)
    func sdkDefaultPubkyClientConfig(
        resolve: RCTPromiseResolveBlock,
        reject: RCTPromiseRejectBlock
    ) {
        resolveResult(resolve) {
            try pubkyClientConfigJson(defaultPubkyClientConfig())
        }
    }

    @objc(sdkRequiredSessionCapabilities:withResolver:withRejecter:)
    func sdkRequiredSessionCapabilities(
        configJson: String,
        resolve: RCTPromiseResolveBlock,
        reject: RCTPromiseRejectBlock
    ) {
        resolveResult(resolve) {
            try requiredSessionCapabilities(config: try configFromJson(configJson))
        }
    }

    @objc(sdkPubkyPublicKeyFromBip39Seed:withResolver:withRejecter:)
    func sdkPubkyPublicKeyFromBip39Seed(
        seedBase64: String,
        resolve: RCTPromiseResolveBlock,
        reject: RCTPromiseRejectBlock
    ) {
        resolveResult(resolve) {
            let seed = try dataFromBase64(seedBase64, label: "seed")
            let secret = try pubkySecretKeyFromBip39Seed(seed: seed)
            return try pubkyPublicKeyFromSecret(localSecretKey: secret)
        }
    }

    @objc(sdkPubkyPublicKeyFromBip39Mnemonic:withResolver:withRejecter:)
    func sdkPubkyPublicKeyFromBip39Mnemonic(
        mnemonicPhrase: String,
        resolve: RCTPromiseResolveBlock,
        reject: RCTPromiseRejectBlock
    ) {
        resolveResult(resolve) {
            let secret = try pubkySecretKeyFromBip39Mnemonic(mnemonicPhrase: mnemonicPhrase)
            return try pubkyPublicKeyFromSecret(localSecretKey: secret)
        }
    }

    @objc(sdkParsePubkyAuthUrl:withResolver:withRejecter:)
    func sdkParsePubkyAuthUrl(
        authUrl: String,
        resolve: RCTPromiseResolveBlock,
        reject: RCTPromiseRejectBlock
    ) {
        resolveResult(resolve) {
            try authDetailsJson(parsePubkyAuthUrl(authUrl: authUrl))
        }
    }

    @objc(sdkResolvePubkyUrl:withResolver:withRejecter:)
    func sdkResolvePubkyUrl(
        uri: String,
        resolve: RCTPromiseResolveBlock,
        reject: RCTPromiseRejectBlock
    ) {
        resolveResult(resolve) {
            try resolvePubkyUrl(uri: uri)
        }
    }

    @objc(sdkParsePubkyResource:withResolver:withRejecter:)
    func sdkParsePubkyResource(
        uri: String,
        resolve: RCTPromiseResolveBlock,
        reject: RCTPromiseRejectBlock
    ) {
        resolveResult(resolve) {
            try resourceRefJson(parsePubkyResource(uri: uri))
        }
    }
}
