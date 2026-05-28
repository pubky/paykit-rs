import Foundation
import React

@objc(Paykit)
class Paykit: RCTEventEmitter {

    override init() {
        super.init()
    }

    @objc override static func requiresMainQueueSetup() -> Bool {
        return false
    }

    override func supportedEvents() -> [String]! {
        return ["PaykitEvent"]
    }

    // MARK: - Helpers

    private func resultArray(_ value: String) -> [String] {
        return ["ok", value]
    }

    private func errorArray(_ message: String) -> [String] {
        return ["error", message]
    }

    private func jsonObject(from jsonString: String, label: String) throws -> [String: Any] {
        let data = Data(jsonString.utf8)
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(label) must be a JSON object"
            ])
        }
        return object
    }

    private func jsonString(_ object: Any, fallback: String) throws -> String {
        let data = try JSONSerialization.data(withJSONObject: object)
        return String(data: data, encoding: .utf8) ?? fallback
    }

    private func nullableString(_ value: String?) -> Any {
        return value ?? NSNull()
    }

    private func requiredString(_ object: [String: Any], key: String) throws -> String {
        guard let value = object[key] as? String else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(key) must be a string"
            ])
        }
        return value
    }

    private func optionalString(_ object: [String: Any], key: String) throws -> String? {
        guard let value = object[key], !(value is NSNull) else {
            return nil
        }
        guard let string = value as? String else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(key) must be a string or null"
            ])
        }
        return string
    }

    private func paymentEndpoints(from raw: Any?) throws -> [FfiPaymentEndpoint] {
        guard let raw = raw, !(raw is NSNull), let paymentEndpoints = raw as? [[String: Any]] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "payment_endpoints must be a JSON array"
            ])
        }
        return try paymentEndpoints.map { item in
            guard let paymentEndpointIdentifier = item["payment_endpoint_identifier"] as? String,
                  let paymentEndpointPayload = item["payment_endpoint_payload"] as? String else {
                throw NSError(domain: "Paykit", code: 1, userInfo: [
                    NSLocalizedDescriptionKey: "payment_endpoints must include string payment_endpoint_identifier and payment_endpoint_payload fields"
                ])
            }
            return FfiPaymentEndpoint(
                paymentEndpointIdentifier: paymentEndpointIdentifier,
                paymentEndpointPayload: paymentEndpointPayload
            )
        }
    }

    private func paymentEndpointsJsonObject(_ paymentEndpoints: [FfiPaymentEndpoint]) -> [[String: String]] {
        return paymentEndpoints.map { paymentEndpoint in
            [
                "payment_endpoint_identifier": paymentEndpoint.paymentEndpointIdentifier,
                "payment_endpoint_payload": paymentEndpoint.paymentEndpointPayload,
            ]
        }
    }

    private func paymentEndpointsJson(_ paymentEndpoints: [FfiPaymentEndpoint]) throws -> String {
        return try jsonString(paymentEndpointsJsonObject(paymentEndpoints), fallback: "[]")
    }

    private func privatePaymentEnvelope(from jsonString: String) throws -> FfiPrivatePaymentEnvelope {
        let object = try jsonObject(from: jsonString, label: "Private Payment Envelope")
        return FfiPrivatePaymentEnvelope(
            reference: try requiredString(object, key: "reference"),
            paymentEndpoints: try paymentEndpoints(from: object["payment_endpoints"])
        )
    }

    private func privatePaymentEnvelopeJson(_ envelope: FfiPrivatePaymentEnvelope?) throws -> String {
        guard let envelope = envelope else {
            return "null"
        }
        return try jsonString([
            "reference": envelope.reference,
            "payment_endpoints": paymentEndpointsJsonObject(envelope.paymentEndpoints),
        ], fallback: "{}")
    }

    private func receiptMetadataFields(from raw: Any?) throws -> [FfiReceiptMetadataField] {
        guard let raw = raw, !(raw is NSNull) else {
            return []
        }
        guard let metadata = raw as? [[String: Any]] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "Receipt Metadata must be a JSON array"
            ])
        }
        return try metadata.map { item in
            guard let key = item["key"] as? String,
                  let value = item["value"] as? String else {
                throw NSError(domain: "Paykit", code: 1, userInfo: [
                    NSLocalizedDescriptionKey: "Receipt Metadata fields must include string key and value fields"
                ])
            }
            return FfiReceiptMetadataField(key: key, value: value)
        }
    }

    private func receiptMetadataJsonObject(_ metadata: [FfiReceiptMetadataField]) -> [[String: String]] {
        return metadata.map { field in
            ["key": field.key, "value": field.value]
        }
    }

    private func receiptDraft(from jsonString: String) throws -> FfiReceiptDraft {
        let object = try jsonObject(from: jsonString, label: "receipt draft")
        return FfiReceiptDraft(
            reference: try requiredString(object, key: "reference"),
            paymentEndpointIdentifier: try optionalString(object, key: "payment_endpoint_identifier"),
            amount: try optionalString(object, key: "amount"),
            currency: try optionalString(object, key: "currency"),
            metadata: try receiptMetadataFields(from: object["metadata"])
        )
    }

    private func issuedReceiptJson(_ receipt: FfiIssuedReceipt) throws -> String {
        return try jsonString([
            "reference": receipt.reference,
            "location": receipt.location,
            "key": receipt.key,
        ], fallback: "{}")
    }

    private func receiptAccessJsonObject(_ access: FfiReceiptAccess) -> [String: Any] {
        return [
            "version": Int(access.version),
            "reference": access.reference,
            "location": access.location,
            "key": access.key,
            "algorithm": access.algorithm,
        ]
    }

    private func receiptAccessJson(_ access: [FfiReceiptAccess]) throws -> String {
        return try jsonString(access.map { receiptAccessJsonObject($0) }, fallback: "[]")
    }

    private func receiptJson(_ receipt: FfiReceipt) throws -> String {
        return try jsonString([
            "reference": receipt.reference,
            "recipient_public_key": receipt.recipientPublicKey,
            "payment_endpoint_identifier": nullableString(receipt.paymentEndpointIdentifier),
            "amount": nullableString(receipt.amount),
            "currency": nullableString(receipt.currency),
            "metadata": receiptMetadataJsonObject(receipt.metadata),
        ], fallback: "{}")
    }

    private func progressJson(_ progress: FfiHandshakeProgress) throws -> String {
        return try jsonString([
            "status": progress.status,
            "handle_id": progress.handleId,
        ], fallback: "{}")
    }

    private func uint32(_ value: NSNumber, label: String) throws -> UInt32 {
        let intValue = value.int64Value
        guard intValue >= 0 && intValue <= Int64(UInt32.max) else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(label) must be between 0 and \(UInt32.max)"
            ])
        }
        return UInt32(intValue)
    }

    // MARK: - Initialization

    @objc(initialize:withRejecter:)
    func initialize(_ resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitInitialize()
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    // MARK: - Session queries

    @objc(isAuthenticated:withRejecter:)
    func isAuthenticated(_ resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            let result = await paykitIsAuthenticated()
            resolve(self.resultArray(result ? "true" : "false"))
        }
    }

    @objc(getCurrentPublicKey:withRejecter:)
    func getCurrentPublicKey(_ resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            let result = await paykitGetCurrentPublicKey()
            resolve(self.resultArray(result ?? ""))
        }
    }

    @objc(exportSession:withRejecter:)
    func exportSession(_ resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let result = try await paykitExportSession()
                resolve(self.resultArray(result))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    // MARK: - Authentication

    @objc(importSession:withResolver:withRejecter:)
    func importSession(_ sessionSecret: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let result = try await paykitImportSession(sessionSecret: sessionSecret)
                resolve(self.resultArray(result))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(signUp:homeserverPublicKey:withResolver:withRejecter:)
    func signUp(_ secretKeyHex: String, homeserverPublicKey: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let result = try await paykitSignUp(secretKeyHex: secretKeyHex, homeserverPublicKey: homeserverPublicKey)
                resolve(self.resultArray(result))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(signIn:withResolver:withRejecter:)
    func signIn(_ secretKeyHex: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let result = try await paykitSignIn(secretKeyHex: secretKeyHex)
                resolve(self.resultArray(result))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(signOut:withRejecter:)
    func signOut(_ resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitSignOut()
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(forceSignOut:withRejecter:)
    func forceSignOut(_ resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            await paykitForceSignOut()
            resolve(self.resultArray(""))
        }
    }

    // MARK: - Payment List (read)

    @objc(getPaymentList:withResolver:withRejecter:)
    func getPaymentList(_ publicKey: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let paymentEndpoints = try await paykitGetPaymentList(publicKey: publicKey)
                resolve(self.resultArray(try self.paymentEndpointsJson(paymentEndpoints)))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(getPaymentEndpoint:paymentEndpointIdentifier:withResolver:withRejecter:)
    func getPaymentEndpoint(_ publicKey: String, paymentEndpointIdentifier: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let result = try await paykitGetPaymentEndpoint(publicKey: publicKey, paymentEndpointIdentifier: paymentEndpointIdentifier)
                resolve(self.resultArray(result ?? ""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    // MARK: - Payment endpoints (write)

    @objc(setPaymentEndpoint:paymentEndpointPayload:withResolver:withRejecter:)
    func setPaymentEndpoint(_ paymentEndpointIdentifier: String, paymentEndpointPayload: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitSetPaymentEndpoint(paymentEndpointIdentifier: paymentEndpointIdentifier, paymentEndpointPayload: paymentEndpointPayload)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(removePaymentEndpoint:withResolver:withRejecter:)
    func removePaymentEndpoint(_ paymentEndpointIdentifier: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitRemovePaymentEndpoint(paymentEndpointIdentifier: paymentEndpointIdentifier)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    // MARK: - Private encrypted payments

    @objc(defaultMaxSendRetries:withRejecter:)
    func defaultMaxSendRetries(_ resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        resolve(self.resultArray(String(paykitDefaultMaxSendRetries())))
    }

    @objc(defaultMaxRecoveryAttempts:withRejecter:)
    func defaultMaxRecoveryAttempts(_ resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        resolve(self.resultArray(String(paykitDefaultMaxRecoveryAttempts())))
    }

    @objc(generatePaymentReference:withRejecter:)
    func generatePaymentReference(_ resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        resolve(self.resultArray(paykitGeneratePaymentReference()))
    }

    @objc(initiateEncryptedLink:receiverPublicKey:withResolver:withRejecter:)
    func initiateEncryptedLink(_ secretKeyHex: String, receiverPublicKey: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let handle = try await paykitInitiateEncryptedLink(secretKeyHex: secretKeyHex, receiverPublicKey: receiverPublicKey)
                resolve(self.resultArray(handle))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(acceptEncryptedLink:senderPublicKey:withResolver:withRejecter:)
    func acceptEncryptedLink(_ secretKeyHex: String, senderPublicKey: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let handle = try await paykitAcceptEncryptedLink(secretKeyHex: secretKeyHex, senderPublicKey: senderPublicKey)
                resolve(self.resultArray(handle))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(advanceHandshake:withResolver:withRejecter:)
    func advanceHandshake(_ handshakeId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let progress = try await paykitAdvanceHandshake(handshakeId: handshakeId)
                resolve(self.resultArray(try self.progressJson(progress)))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(setEncryptedLinkHandshakeMaxRecoveryAttempts:max:withResolver:withRejecter:)
    func setEncryptedLinkHandshakeMaxRecoveryAttempts(_ handshakeId: String, max: NSNumber, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitSetEncryptedLinkHandshakeMaxRecoveryAttempts(handshakeId: handshakeId, max: try self.uint32(max, label: "max recovery attempts"))
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(setEncryptedLinkMaxSendRetries:max:withResolver:withRejecter:)
    func setEncryptedLinkMaxSendRetries(_ linkId: String, max: NSNumber, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitSetEncryptedLinkMaxSendRetries(linkId: linkId, max: try self.uint32(max, label: "max send retries"))
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(setPrivatePaymentEnvelope:payloadJson:withResolver:withRejecter:)
    func setPrivatePaymentEnvelope(_ linkId: String, payloadJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let envelope = try self.privatePaymentEnvelope(from: payloadJson)
                try await paykitSetPrivatePaymentEnvelope(linkId: linkId, envelope: envelope)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(getPrivatePaymentEnvelope:withResolver:withRejecter:)
    func getPrivatePaymentEnvelope(_ linkId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let envelope = try await paykitGetPrivatePaymentEnvelope(linkId: linkId)
                resolve(self.resultArray(try self.privatePaymentEnvelopeJson(envelope)))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(issueReceipt:draftJson:withResolver:withRejecter:)
    func issueReceipt(_ linkId: String, draftJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let draft = try self.receiptDraft(from: draftJson)
                let receipt = try await paykitIssueReceipt(linkId: linkId, draft: draft)
                resolve(self.resultArray(try self.issuedReceiptJson(receipt)))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(getReceiptAccess:withResolver:withRejecter:)
    func getReceiptAccess(_ linkId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let access = try await paykitGetReceiptAccess(linkId: linkId)
                resolve(self.resultArray(try self.receiptAccessJson(access)))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(receiptLocation:withResolver:withRejecter:)
    func receiptLocation(_ reference: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let location = try paykitReceiptLocation(reference: reference)
            resolve(self.resultArray(location))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(decryptReceipt:key:location:withResolver:withRejecter:)
    func decryptReceipt(_ encryptedJson: String, key: String, location: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let receipt = try paykitDecryptReceipt(encryptedJson: encryptedJson, key: key, location: location)
            resolve(self.resultArray(try self.receiptJson(receipt)))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(serializeEncryptedLinkHandshake:withResolver:withRejecter:)
    func serializeEncryptedLinkHandshake(_ handshakeId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let snapshot = try await paykitSerializeEncryptedLinkHandshake(handshakeId: handshakeId)
                resolve(self.resultArray(snapshot))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(serializeEncryptedLink:withResolver:withRejecter:)
    func serializeEncryptedLink(_ linkId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let snapshot = try await paykitSerializeEncryptedLink(linkId: linkId)
                resolve(self.resultArray(snapshot))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(encryptedLinkSnapshotRecipient:withResolver:withRejecter:)
    func encryptedLinkSnapshotRecipient(_ snapshotHex: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let recipient = try paykitEncryptedLinkSnapshotRecipient(snapshotHex: snapshotHex)
            resolve(self.resultArray(recipient))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(encryptedLinkHandshakeSnapshotRecipient:withResolver:withRejecter:)
    func encryptedLinkHandshakeSnapshotRecipient(_ snapshotHex: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let recipient = try paykitEncryptedLinkHandshakeSnapshotRecipient(snapshotHex: snapshotHex)
            resolve(self.resultArray(recipient))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(restoreEncryptedLink:snapshotHex:withResolver:withRejecter:)
    func restoreEncryptedLink(_ secretKeyHex: String, snapshotHex: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let handle = try await paykitRestoreEncryptedLink(secretKeyHex: secretKeyHex, snapshotHex: snapshotHex)
                resolve(self.resultArray(handle))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(restoreEncryptedLinkHandshake:snapshotHex:withResolver:withRejecter:)
    func restoreEncryptedLinkHandshake(_ secretKeyHex: String, snapshotHex: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let handle = try await paykitRestoreEncryptedLinkHandshake(secretKeyHex: secretKeyHex, snapshotHex: snapshotHex)
                resolve(self.resultArray(handle))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(closeEncryptedLink:withResolver:withRejecter:)
    func closeEncryptedLink(_ linkId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitCloseEncryptedLink(linkId: linkId)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(dropEncryptedLinkHandshake:withResolver:withRejecter:)
    func dropEncryptedLinkHandshake(_ handshakeId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitDropEncryptedLinkHandshake(handshakeId: handshakeId)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }
}
