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

    private func optionalResultArray(_ value: String?) -> [Any] {
        return ["ok", value ?? NSNull()]
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

    private func requireAllowedKeys(
        _ object: [String: Any],
        label: String,
        allowedKeys: Set<String>
    ) throws {
        for key in object.keys where !allowedKeys.contains(key) {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(label) contains unsupported field '\(key)'"
            ])
        }
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

    private func requiredObject(_ object: [String: Any], key: String) throws -> [String: Any] {
        guard let value = object[key] as? [String: Any] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(key) must be a JSON object"
            ])
        }
        return value
    }

    private func requiredStringArray(_ object: [String: Any], key: String) throws -> [String] {
        guard let value = object[key] as? [String] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(key) must be an array of strings"
            ])
        }
        return value
    }

    private func jsonObjectString(_ object: [String: Any], fallback: String = "{}") throws -> String {
        try jsonString(object, fallback: fallback)
    }

    private func paymentEndpoints(from raw: Any?) throws -> [FfiPaymentEndpoint] {
        guard let raw = raw, !(raw is NSNull), let paymentEndpoints = raw as? [[String: Any]] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "payment_endpoints must be a JSON array"
            ])
        }
        return try paymentEndpoints.map { item in
            try requireAllowedKeys(
                item,
                label: "Payment Endpoint",
                allowedKeys: ["payment_endpoint_identifier", "payment_endpoint_payload"]
            )
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

    private func privatePaymentList(from jsonString: String) throws -> FfiPrivatePaymentList {
        let object = try jsonObject(from: jsonString, label: "Private Payment List")
        try requireAllowedKeys(
            object,
            label: "Private Payment List",
            allowedKeys: ["payment_endpoints"]
        )
        return FfiPrivatePaymentList(
            paymentEndpoints: try paymentEndpoints(from: object["payment_endpoints"])
        )
    }

    private func receiptMetadataJsonObject(_ metadataJson: String) throws -> [String: Any] {
        let data = Data(metadataJson.utf8)
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "Receipt metadata must be a JSON object"
            ])
        }
        return object
    }

    private func receiptMetadataJson(from raw: Any?) throws -> String {
        guard let raw = raw, !(raw is NSNull) else {
            return "{}"
        }
        guard let metadata = raw as? [String: Any] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "metadata must be a JSON object or null"
            ])
        }
        return try jsonString(metadata, fallback: "{}")
    }

    private func receiptDraft(from jsonString: String) throws -> FfiReceiptDraft {
        let object = try jsonObject(from: jsonString, label: "Receipt Draft")
        try requireAllowedKeys(
            object,
            label: "Receipt Draft",
            allowedKeys: [
                "receipt_id",
                "payment_reference",
                "payment_request_id",
                "billing_period",
                "payment_endpoint_identifier",
                "amount",
                "metadata",
            ]
        )
        return FfiReceiptDraft(
            receiptId: try optionalString(object, key: "receipt_id"),
            paymentReference: try requiredString(object, key: "payment_reference"),
            paymentRequestId: try optionalString(object, key: "payment_request_id"),
            billingPeriod: try billingPeriod(from: object["billing_period"], key: "billing_period"),
            paymentEndpointIdentifier: try optionalString(object, key: "payment_endpoint_identifier"),
            amount: try paymentAmount(from: object["amount"], key: "amount"),
            metadataJson: try receiptMetadataJson(from: object["metadata"])
        )
    }

    private func privateApplicationMessagesJson(_ messages: [FfiPrivateApplicationMessage]) throws -> String {
        return try jsonString(messages.map { message in
            let version: Any = message.version.map { Int($0) } ?? NSNull()
            let kind: Any = message.kind ?? NSNull()
            [
                "version": version,
                "kind": kind,
                "raw_json": message.rawJson,
            ]
        }, fallback: "[]")
    }

    private func receiptJson(_ receipt: FfiReceipt) throws -> String {
        return try jsonString([
            "receipt_id": receipt.receiptId,
            "payment_reference": receipt.paymentReference,
            "payment_request_id": nullableString(receipt.paymentRequestId),
            "billing_period": billingPeriodJsonObject(receipt.billingPeriod),
            "recipient_public_key": receipt.recipientPublicKey,
            "payment_endpoint_identifier": nullableString(receipt.paymentEndpointIdentifier),
            "amount": paymentAmountJsonObject(receipt.amount),
            "metadata": try receiptMetadataJsonObject(receipt.metadataJson),
        ], fallback: "{}")
    }

    private func paymentAmountJsonObject(_ amount: FfiPaymentAmount?) -> Any {
        guard let amount else {
            return NSNull()
        }
        return [
            "value": amount.value,
            "asset": amount.asset,
        ]
    }

    private func paymentAmount(from raw: Any?, key: String) throws -> FfiPaymentAmount? {
        guard let raw = raw, !(raw is NSNull) else {
            return nil
        }
        guard let object = raw as? [String: Any] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(key) must be a JSON object or null"
            ])
        }
        try requireAllowedKeys(
            object,
            label: "Payment Amount",
            allowedKeys: ["value", "asset"]
        )
        return FfiPaymentAmount(
            value: try requiredString(object, key: "value"),
            asset: try requiredString(object, key: "asset")
        )
    }

    private func billingPeriodJsonObject(_ period: FfiBillingPeriod?) -> Any {
        guard let period else {
            return NSNull()
        }
        return [
            "starts_at": period.startsAt,
            "ends_at": period.endsAt,
        ]
    }

    private func billingPeriod(from raw: Any?, key: String) throws -> FfiBillingPeriod? {
        guard let raw = raw, !(raw is NSNull) else {
            return nil
        }
        guard let object = raw as? [String: Any] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(key) must be a JSON object or null"
            ])
        }
        try requireAllowedKeys(
            object,
            label: "Billing Period",
            allowedKeys: ["starts_at", "ends_at"]
        )
        return FfiBillingPeriod(
            startsAt: try requiredString(object, key: "starts_at"),
            endsAt: try requiredString(object, key: "ends_at")
        )
    }

    private func privateApplicationMessage(from jsonString: String) throws -> FfiPrivateApplicationMessage {
        let object = try jsonObject(from: jsonString, label: "Private Application Message")
        try requireAllowedKeys(
            object,
            label: "Private Application Message",
            allowedKeys: ["version", "kind", "raw_json"]
        )
        let version: UInt32?
        if let rawVersion = object["version"], !(rawVersion is NSNull) {
            guard let number = rawVersion as? NSNumber else {
                throw NSError(domain: "Paykit", code: 1, userInfo: [
                    NSLocalizedDescriptionKey: "version must be a number or null"
                ])
            }
            version = try uint32(number, label: "version")
        } else {
            version = nil
        }
        return FfiPrivateApplicationMessage(
            version: version,
            kind: try optionalString(object, key: "kind"),
            rawJson: try requiredString(object, key: "raw_json")
        )
    }

    private func recurrenceJsonObject(_ recurrence: FfiRecurrence?) -> Any {
        guard let recurrence else {
            return NSNull()
        }
        return [
            "every": Int(recurrence.every),
            "unit": recurrence.unit,
            "starts_at": recurrence.startsAt,
            "anchor": recurrence.anchor,
            "ends_at": nullableString(recurrence.endsAt),
        ]
    }

    private func recurrence(from raw: Any?) throws -> FfiRecurrence? {
        guard let raw = raw, !(raw is NSNull) else {
            return nil
        }
        guard let object = raw as? [String: Any] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "recurrence must be a JSON object or null"
            ])
        }
        try requireAllowedKeys(
            object,
            label: "Recurrence",
            allowedKeys: ["every", "unit", "starts_at", "anchor", "ends_at"]
        )
        guard let every = object["every"] as? NSNumber else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "every must be a number"
            ])
        }
        return FfiRecurrence(
            every: try uint32(every, label: "every"),
            unit: try requiredString(object, key: "unit"),
            startsAt: try requiredString(object, key: "starts_at"),
            anchor: try requiredString(object, key: "anchor"),
            endsAt: try optionalString(object, key: "ends_at")
        )
    }

    private func paymentRequestTermsJsonObject(_ terms: FfiPaymentRequestTerms) throws -> [String: Any] {
        [
            "amount": paymentAmountJsonObject(terms.amount),
            "payment_reference": terms.paymentReference,
            "proposal_expires_at": nullableString(terms.proposalExpiresAt),
            "recurrence": recurrenceJsonObject(terms.recurrence),
            "accepted_payment_endpoint_identifiers": terms.acceptedPaymentEndpointIdentifiers,
            "metadata": try receiptMetadataJsonObject(terms.metadataJson),
        ]
    }

    private func paymentRequestTerms(from object: [String: Any]) throws -> FfiPaymentRequestTerms {
        try requireAllowedKeys(
            object,
            label: "Payment Request terms",
            allowedKeys: [
                "amount",
                "payment_reference",
                "proposal_expires_at",
                "recurrence",
                "accepted_payment_endpoint_identifiers",
                "metadata",
            ]
        )
        guard let amount = try paymentAmount(from: object["amount"], key: "amount") else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "amount is required"
            ])
        }
        return FfiPaymentRequestTerms(
            amount: amount,
            paymentReference: try requiredString(object, key: "payment_reference"),
            proposalExpiresAt: try optionalString(object, key: "proposal_expires_at"),
            recurrence: try recurrence(from: object["recurrence"]),
            acceptedPaymentEndpointIdentifiers: try requiredStringArray(
                object,
                key: "accepted_payment_endpoint_identifiers"
            ),
            metadataJson: try receiptMetadataJson(from: object["metadata"])
        )
    }

    private func paymentRequestJsonObject(_ event: FfiPaymentRequest) throws -> [String: Any] {
        [
            "event_id": event.eventId,
            "payment_request_id": event.paymentRequestId,
            "request": try paymentRequestTermsJsonObject(event.request),
        ]
    }

    private func paymentRequest(from object: [String: Any]) throws -> FfiPaymentRequest {
        try requireAllowedKeys(
            object,
            label: "Payment Request",
            allowedKeys: ["event_id", "payment_request_id", "request"]
        )
        return FfiPaymentRequest(
            eventId: try requiredString(object, key: "event_id"),
            paymentRequestId: try requiredString(object, key: "payment_request_id"),
            request: try paymentRequestTerms(from: requiredObject(object, key: "request"))
        )
    }

    private func paymentRequestAcceptanceJsonObject(_ event: FfiPaymentRequestAcceptance) -> [String: Any] {
        [
            "event_id": event.eventId,
            "payment_request_id": event.paymentRequestId,
        ]
    }

    private func paymentRequestAcceptance(from object: [String: Any]) throws -> FfiPaymentRequestAcceptance {
        try requireAllowedKeys(
            object,
            label: "Payment Request Acceptance",
            allowedKeys: ["event_id", "payment_request_id"]
        )
        return FfiPaymentRequestAcceptance(
            eventId: try requiredString(object, key: "event_id"),
            paymentRequestId: try requiredString(object, key: "payment_request_id")
        )
    }

    private func paymentRequestRejectionJsonObject(_ event: FfiPaymentRequestRejection) -> [String: Any] {
        [
            "event_id": event.eventId,
            "payment_request_id": event.paymentRequestId,
            "reason": nullableString(event.reason),
        ]
    }

    private func paymentRequestRejection(from object: [String: Any]) throws -> FfiPaymentRequestRejection {
        try requireAllowedKeys(
            object,
            label: "Payment Request Rejection",
            allowedKeys: ["event_id", "payment_request_id", "reason"]
        )
        return FfiPaymentRequestRejection(
            eventId: try requiredString(object, key: "event_id"),
            paymentRequestId: try requiredString(object, key: "payment_request_id"),
            reason: try optionalString(object, key: "reason")
        )
    }

    private func paymentRequestCancellationJsonObject(_ event: FfiPaymentRequestCancellation) -> [String: Any] {
        [
            "event_id": event.eventId,
            "payment_request_id": event.paymentRequestId,
            "reason": nullableString(event.reason),
        ]
    }

    private func paymentRequestCancellation(from object: [String: Any]) throws -> FfiPaymentRequestCancellation {
        try requireAllowedKeys(
            object,
            label: "Payment Request Cancellation",
            allowedKeys: ["event_id", "payment_request_id", "reason"]
        )
        return FfiPaymentRequestCancellation(
            eventId: try requiredString(object, key: "event_id"),
            paymentRequestId: try requiredString(object, key: "payment_request_id"),
            reason: try optionalString(object, key: "reason")
        )
    }

    private func paymentProofJsonObject(_ proof: FfiPaymentProof) throws -> [String: Any] {
        [
            "event_id": proof.eventId,
            "payment_request_id": proof.paymentRequestId,
            "payment_reference": proof.paymentReference,
            "billing_period": billingPeriodJsonObject(proof.billingPeriod),
            "payment_endpoint_identifier": proof.paymentEndpointIdentifier,
            "proof": try receiptMetadataJsonObject(proof.proofJson),
        ]
    }

    private func paymentProof(from object: [String: Any]) throws -> FfiPaymentProof {
        try requireAllowedKeys(
            object,
            label: "Payment Proof",
            allowedKeys: [
                "event_id",
                "payment_request_id",
                "payment_reference",
                "billing_period",
                "payment_endpoint_identifier",
                "proof",
            ]
        )
        return FfiPaymentProof(
            eventId: try requiredString(object, key: "event_id"),
            paymentRequestId: try requiredString(object, key: "payment_request_id"),
            paymentReference: try requiredString(object, key: "payment_reference"),
            billingPeriod: try billingPeriod(from: object["billing_period"], key: "billing_period"),
            paymentEndpointIdentifier: try requiredString(object, key: "payment_endpoint_identifier"),
            proofJson: try jsonObjectString(requiredObject(object, key: "proof"))
        )
    }

    private func paymentRequestEventJsonObject(_ event: FfiPaymentRequestEvent) throws -> [String: Any] {
        [
            "event_type": event.eventType,
            "request": try event.request.map(paymentRequestJsonObject) ?? NSNull(),
            "acceptance": event.acceptance.map(paymentRequestAcceptanceJsonObject) ?? NSNull(),
            "rejection": event.rejection.map(paymentRequestRejectionJsonObject) ?? NSNull(),
            "cancellation": event.cancellation.map(paymentRequestCancellationJsonObject) ?? NSNull(),
            "proof": try event.proof.map(paymentProofJsonObject) ?? NSNull(),
        ]
    }

    private func paymentRequestEvent(from object: [String: Any]) throws -> FfiPaymentRequestEvent {
        try requireAllowedKeys(
            object,
            label: "Payment Request Event",
            allowedKeys: ["event_type", "request", "acceptance", "rejection", "cancellation", "proof"]
        )
        return FfiPaymentRequestEvent(
            eventType: try requiredString(object, key: "event_type"),
            request: try (object["request"] as? [String: Any]).map(paymentRequest),
            acceptance: try (object["acceptance"] as? [String: Any]).map(paymentRequestAcceptance),
            rejection: try (object["rejection"] as? [String: Any]).map(paymentRequestRejection),
            cancellation: try (object["cancellation"] as? [String: Any]).map(paymentRequestCancellation),
            proof: try (object["proof"] as? [String: Any]).map(paymentProof)
        )
    }

    private func paymentRequestEventMessageJson(_ message: FfiPaymentRequestEventMessage?) throws -> String {
        guard let message else {
            return "null"
        }
        return try jsonString([
            "kind": message.kind,
            "event_id": nullableString(message.eventId),
            "payment_request_id": nullableString(message.paymentRequestId),
            "raw_json": message.rawJson,
            "event": try message.event.map(paymentRequestEventJsonObject) ?? NSNull(),
            "validation_error": nullableString(message.validationError),
        ], fallback: "{}")
    }

    private func receiptAccessEventMessageJson(_ message: FfiReceiptAccessEventMessage?) throws -> String {
        guard let message else {
            return "null"
        }
        return try jsonString([
            "kind": message.kind,
            "event_id": nullableString(message.eventId),
            "receipt_id": nullableString(message.receiptId),
            "raw_json": message.rawJson,
            "access": message.access.map(receiptAccessJsonObject) ?? NSNull(),
            "validation_error": nullableString(message.validationError),
        ], fallback: "{}")
    }

    private func receipt(from object: [String: Any]) throws -> FfiReceipt {
        try requireAllowedKeys(
            object,
            label: "Receipt",
            allowedKeys: [
                "receipt_id",
                "payment_reference",
                "payment_request_id",
                "billing_period",
                "recipient_public_key",
                "payment_endpoint_identifier",
                "amount",
                "metadata",
            ]
        )
        FfiReceipt(
            receiptId: try requiredString(object, key: "receipt_id"),
            paymentReference: try requiredString(object, key: "payment_reference"),
            paymentRequestId: try optionalString(object, key: "payment_request_id"),
            billingPeriod: try billingPeriod(from: object["billing_period"], key: "billing_period"),
            recipientPublicKey: try requiredString(object, key: "recipient_public_key"),
            paymentEndpointIdentifier: try optionalString(object, key: "payment_endpoint_identifier"),
            amount: try paymentAmount(from: object["amount"], key: "amount"),
            metadataJson: try receiptMetadataJson(from: object["metadata"])
        )
    }

    private func receiptAccessJsonObject(_ access: FfiReceiptAccess) -> [String: Any] {
        [
            "event_id": access.eventId,
            "receipt_id": access.receiptId,
            "payment_reference": access.paymentReference,
            "payment_request_id": nullableString(access.paymentRequestId),
            "billing_period": billingPeriodJsonObject(access.billingPeriod),
            "location": access.location,
            "key": access.key,
        ]
    }

    private func receiptAccess(from object: [String: Any]) throws -> FfiReceiptAccess {
        try requireAllowedKeys(
            object,
            label: "Receipt Access",
            allowedKeys: [
                "event_id",
                "receipt_id",
                "payment_reference",
                "payment_request_id",
                "billing_period",
                "location",
                "key",
            ]
        )
        FfiReceiptAccess(
            eventId: try requiredString(object, key: "event_id"),
            receiptId: try requiredString(object, key: "receipt_id"),
            paymentReference: try requiredString(object, key: "payment_reference"),
            paymentRequestId: try optionalString(object, key: "payment_request_id"),
            billingPeriod: try billingPeriod(from: object["billing_period"], key: "billing_period"),
            location: try requiredString(object, key: "location"),
            key: try requiredString(object, key: "key")
        )
    }

    private func preparedReceiptJson(_ prepared: FfiPreparedReceipt) throws -> String {
        try jsonString([
            "receipt": try JSONSerialization.jsonObject(with: Data(receiptJson(prepared.receipt).utf8)),
            "encrypted_receipt": prepared.encryptedReceipt,
            "access": receiptAccessJsonObject(prepared.access),
        ], fallback: "{}")
    }

    private func preparedReceipt(from jsonString: String) throws -> FfiPreparedReceipt {
        let object = try jsonObject(from: jsonString, label: "Prepared Receipt")
        try requireAllowedKeys(
            object,
            label: "Prepared Receipt",
            allowedKeys: ["receipt", "encrypted_receipt", "access"]
        )
        guard let receiptObject = object["receipt"] as? [String: Any],
              let accessObject = object["access"] as? [String: Any] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "Prepared Receipt must contain receipt and access objects"
            ])
        }
        return FfiPreparedReceipt(
            receipt: try receipt(from: receiptObject),
            encryptedReceipt: try requiredString(object, key: "encrypted_receipt"),
            access: try receiptAccess(from: accessObject)
        )
    }

    private func progressJson(_ progress: FfiHandshakeProgress) throws -> String {
        return try jsonString([
            "status": progress.status,
            "handle_id": progress.handleId,
        ], fallback: "{}")
    }

    private func uint32(_ value: NSNumber, label: String) throws -> UInt32 {
        guard CFGetTypeID(value) != CFBooleanGetTypeID() else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(label) must be an integer between 0 and \(UInt32.max)"
            ])
        }
        let doubleValue = value.doubleValue
        guard doubleValue.isFinite,
              doubleValue.rounded(.towardZero) == doubleValue,
              doubleValue >= 0,
              doubleValue <= Double(UInt32.max) else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(label) must be an integer between 0 and \(UInt32.max)"
            ])
        }
        return UInt32(doubleValue)
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
                resolve(self.optionalResultArray(result))
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

    @objc(setPrivatePaymentList:payloadJson:withResolver:withRejecter:)
    func setPrivatePaymentList(_ linkId: String, payloadJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let list = try self.privatePaymentList(from: payloadJson)
                try await paykitSetPrivatePaymentList(linkId: linkId, list: list)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(receivePrivateApplicationMessages:withResolver:withRejecter:)
    func receivePrivateApplicationMessages(_ linkId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let messages = try await paykitReceivePrivateApplicationMessages(linkId: linkId)
                resolve(self.resultArray(try self.privateApplicationMessagesJson(messages)))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(parsePrivatePaymentListJson:withResolver:withRejecter:)
    func parsePrivatePaymentListJson(_ json: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let list = try paykitParsePrivatePaymentListJson(json: json)
            resolve(self.resultArray(try self.jsonString([
                "payment_endpoints": self.paymentEndpointsJsonObject(list.paymentEndpoints)
            ], fallback: "{}")))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(parsePaymentRequestEventMessage:withResolver:withRejecter:)
    func parsePaymentRequestEventMessage(_ messageJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let message = try self.privateApplicationMessage(from: messageJson)
            let eventMessage = try paykitParsePaymentRequestEventMessage(message: message)
            resolve(self.resultArray(try self.paymentRequestEventMessageJson(eventMessage)))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(serializePaymentRequestEvent:withResolver:withRejecter:)
    func serializePaymentRequestEvent(_ eventJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let object = try self.jsonObject(from: eventJson, label: "Payment Request Event")
            let json = try paykitSerializePaymentRequestEvent(event: try self.paymentRequestEvent(from: object))
            resolve(self.resultArray(json))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(validatePaymentProofForRequest:requestJson:withResolver:withRejecter:)
    func validatePaymentProofForRequest(_ proofJson: String, requestJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let proof = try self.paymentProof(from: self.jsonObject(from: proofJson, label: "Payment Proof"))
            let request = try self.paymentRequest(from: self.jsonObject(from: requestJson, label: "Payment Request"))
            try paykitValidatePaymentProofForRequest(proof: proof, request: request)
            resolve(self.resultArray(""))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(sendPaymentRequest:eventJson:withResolver:withRejecter:)
    func sendPaymentRequest(_ linkId: String, eventJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let event = try self.paymentRequest(from: self.jsonObject(from: eventJson, label: "Payment Request"))
                try await paykitSendPaymentRequest(linkId: linkId, event: event)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(sendPaymentRequestAcceptance:eventJson:withResolver:withRejecter:)
    func sendPaymentRequestAcceptance(_ linkId: String, eventJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let event = try self.paymentRequestAcceptance(from: self.jsonObject(from: eventJson, label: "Payment Request Acceptance"))
                try await paykitSendPaymentRequestAcceptance(linkId: linkId, event: event)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(sendPaymentRequestRejection:eventJson:withResolver:withRejecter:)
    func sendPaymentRequestRejection(_ linkId: String, eventJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let event = try self.paymentRequestRejection(from: self.jsonObject(from: eventJson, label: "Payment Request Rejection"))
                try await paykitSendPaymentRequestRejection(linkId: linkId, event: event)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(sendPaymentRequestCancellation:eventJson:withResolver:withRejecter:)
    func sendPaymentRequestCancellation(_ linkId: String, eventJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let event = try self.paymentRequestCancellation(from: self.jsonObject(from: eventJson, label: "Payment Request Cancellation"))
                try await paykitSendPaymentRequestCancellation(linkId: linkId, event: event)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(sendPaymentProof:eventJson:withResolver:withRejecter:)
    func sendPaymentProof(_ linkId: String, eventJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let event = try self.paymentProof(from: self.jsonObject(from: eventJson, label: "Payment Proof"))
                try await paykitSendPaymentProof(linkId: linkId, event: event)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(prepareReceipt:draftJson:withResolver:withRejecter:)
    func prepareReceipt(_ linkId: String, draftJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let prepared = try await paykitPrepareReceipt(
                    linkId: linkId,
                    draft: try self.receiptDraft(from: draftJson)
                )
                resolve(self.resultArray(try self.preparedReceiptJson(prepared)))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(parseReceiptAccessEventMessage:withResolver:withRejecter:)
    func parseReceiptAccessEventMessage(_ messageJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let message = try self.privateApplicationMessage(from: messageJson)
            let eventMessage = try paykitParseReceiptAccessEventMessage(message: message)
            resolve(self.resultArray(try self.receiptAccessEventMessageJson(eventMessage)))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(parseReceiptAccessJson:withResolver:withRejecter:)
    func parseReceiptAccessJson(_ json: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let access = try paykitParseReceiptAccessJson(json: json)
            resolve(self.resultArray(try jsonString(self.receiptAccessJsonObject(access), fallback: "{}")))
        } catch {
            resolve(self.errorArray(error.localizedDescription))
        }
    }

    @objc(storePreparedReceipt:withResolver:withRejecter:)
    func storePreparedReceipt(_ preparedJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitStorePreparedReceipt(prepared: try self.preparedReceipt(from: preparedJson))
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(sendReceiptAccess:accessJson:withResolver:withRejecter:)
    func sendReceiptAccess(_ linkId: String, accessJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let accessObject = try self.jsonObject(from: accessJson, label: "Receipt Access")
                try await paykitSendReceiptAccess(
                    linkId: linkId,
                    access: try self.receiptAccess(from: accessObject)
                )
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(receiptLocation:withResolver:withRejecter:)
    func receiptLocation(_ receiptId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        do {
            let location = try paykitReceiptLocation(receiptId: receiptId)
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
