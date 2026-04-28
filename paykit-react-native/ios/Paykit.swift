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

    private func paymentEntries(from jsonString: String) throws -> [FfiPaymentEntry] {
        let data = Data(jsonString.utf8)
        guard let raw = try JSONSerialization.jsonObject(with: data) as? [[String: Any]] else {
            throw NSError(domain: "Paykit", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "payment entries must be a JSON array"
            ])
        }
        return try raw.map { item in
            guard let methodId = item["method_id"] as? String,
                  let endpointData = item["endpoint_data"] as? String else {
                throw NSError(domain: "Paykit", code: 1, userInfo: [
                    NSLocalizedDescriptionKey: "payment entries must include string method_id and endpoint_data fields"
                ])
            }
            FfiPaymentEntry(
                methodId: methodId,
                endpointData: endpointData
            )
        }
    }

    private func paymentEntriesJson(_ entries: [FfiPaymentEntry]) throws -> String {
        let jsonArray = entries.map { entry in
            ["method_id": entry.methodId, "endpoint_data": entry.endpointData]
        }
        let data = try JSONSerialization.data(withJSONObject: jsonArray)
        return String(data: data, encoding: .utf8) ?? "[]"
    }

    private func progressJson(_ progress: FfiHandshakeProgress) throws -> String {
        let data = try JSONSerialization.data(withJSONObject: [
            "status": progress.status,
            "handle_id": progress.handleId,
        ])
        return String(data: data, encoding: .utf8) ?? "{}"
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

    // MARK: - Payment list (read)

    @objc(getPaymentList:withResolver:withRejecter:)
    func getPaymentList(_ publicKey: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let entries = try await paykitGetPaymentList(publicKey: publicKey)
                resolve(self.resultArray(try self.paymentEntriesJson(entries)))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(getPaymentEndpoint:methodId:withResolver:withRejecter:)
    func getPaymentEndpoint(_ publicKey: String, methodId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let result = try await paykitGetPaymentEndpoint(publicKey: publicKey, methodId: methodId)
                resolve(self.resultArray(result ?? ""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    // MARK: - Payment endpoints (write)

    @objc(setPaymentEndpoint:endpointData:withResolver:withRejecter:)
    func setPaymentEndpoint(_ methodId: String, endpointData: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitSetPaymentEndpoint(methodId: methodId, endpointData: endpointData)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(removePaymentEndpoint:withResolver:withRejecter:)
    func removePaymentEndpoint(_ methodId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                try await paykitRemovePaymentEndpoint(methodId: methodId)
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

    @objc(setPrivatePayments:entriesJson:withResolver:withRejecter:)
    func setPrivatePayments(_ linkId: String, entriesJson: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let entries = try self.paymentEntries(from: entriesJson)
                try await paykitSetPrivatePayments(linkId: linkId, entries: entries)
                resolve(self.resultArray(""))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
        }
    }

    @objc(getPrivatePayments:withResolver:withRejecter:)
    func getPrivatePayments(_ linkId: String, resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
        Task {
            do {
                let entries = try await paykitGetPrivatePayments(linkId: linkId)
                resolve(self.resultArray(try self.paymentEntriesJson(entries)))
            } catch {
                resolve(self.errorArray(error.localizedDescription))
            }
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
