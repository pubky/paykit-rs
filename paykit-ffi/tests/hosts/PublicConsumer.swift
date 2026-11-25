import Foundation
import Paykit

/// The payer's public Payment Endpoints and outbox (Paykit spec 4, Dispatch spec 11).
func checkPublic(fixture: CheckoutFixture, wallet: NativeWallet) async throws {
    let endpoints = try await wallet.ownPublicEndpoints()
    precondition(endpoints.isEmpty)
    let endpoint = PublicEndpoint(identifier: "btc-lightning-bolt11", payload: "lnbc1public")

    let published = try await wallet.setPublicEndpoint(identifier: endpoint.identifier, payload: endpoint.payload)
    let own = try await wallet.ownPublicEndpoints()
    guard case .published = published, own == [endpoint] else {
        preconditionFailure("the public Payment Endpoint was not published")
    }
    let removed = try await wallet.removePublicEndpoint(identifier: endpoint.identifier)
    let withdrawn = try await wallet.ownPublicEndpoints()
    precondition(withdrawn.isEmpty)
    let cleared = try await wallet.setPublicEndpoints(endpoints: [], expiresAtMs: fixture.nowMs())
    let refreshed = try await wallet.updateOwnRecord()
    guard case .published = removed, case .published = cleared, case .published = refreshed else {
        preconditionFailure("the cleared public record was not published")
    }
    let remaining = try await wallet.ownPublicEndpoints()
    precondition(remaining.isEmpty)
    try await wallet.flush()
    let delivery = try await wallet.messageDelivery(msgId: "00000000-0000-4000-8000-000000000000")
    precondition(delivery == nil)
}
