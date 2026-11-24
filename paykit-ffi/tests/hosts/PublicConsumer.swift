import Foundation
import Paykit

/// The payer's public Payment Endpoints and outbox (Paykit spec 4, Switch spec 11).
func checkPublic(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let endpoints = try await wallet.ownPublicEndpoints()
    precondition(endpoints.isEmpty)
    let endpoint = PublicEndpoint(identifier: "btc-lightning-bolt11", payload: "lnbc1public")

    func checkSellingScope(_ publish: () async throws -> PublicationState) async throws {
        do {
            _ = try await publish()
            preconditionFailure("payer published a public Payment Endpoint")
        } catch let WalletError.NeedsUser(failure) {
            precondition(failure.code() == "scope_missing")
            guard case let .scopeMissing(scope)? = failure.refusal()?.kind(), scope == "paykit_request" else {
                preconditionFailure("expected the public Payment Endpoint selling scope")
            }
        }
    }
    try await checkSellingScope {
        try await wallet.setPublicEndpoint(identifier: endpoint.identifier, payload: endpoint.payload)
    }
    try await checkSellingScope {
        try await wallet.setPublicEndpoints(endpoints: [endpoint], expiresAtMs: nil)
    }
    let removed = try await wallet.removePublicEndpoint(identifier: endpoint.identifier)
    let cleared = try await wallet.setPublicEndpoints(endpoints: [], expiresAtMs: fixture.nowMs())
    let refreshed = try await wallet.updateOwnRecord()
    guard case .notPublishing = removed, case .notPublishing = cleared, case .notPublishing = refreshed else {
        preconditionFailure("unexpected payer publication state")
    }
    let remaining = try await wallet.ownPublicEndpoints()
    precondition(remaining.isEmpty)
    try await wallet.flush()
    let delivery = try await wallet.messageDelivery(msgId: "00000000-0000-4000-8000-000000000000")
    precondition(delivery == nil)
}
