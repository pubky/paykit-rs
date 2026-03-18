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
                let jsonArray = entries.map { entry in
                    ["method_id": entry.methodId, "endpoint_data": entry.endpointData]
                }
                let data = try JSONSerialization.data(withJSONObject: jsonArray)
                let jsonString = String(data: data, encoding: .utf8) ?? "[]"
                resolve(self.resultArray(jsonString))
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
}
