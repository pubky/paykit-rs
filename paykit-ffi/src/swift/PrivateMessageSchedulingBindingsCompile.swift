// Compile-only consumer fixture for private message scheduling.
private func compilePrivateMessageSchedulingBindingsSurface(
    sdk: PaykitSdkProtocol
) async throws {
    let snapshot: PrivateMessageSchedulingSnapshot = try await sdk.privateMessageSchedulingSnapshot()
    let peers: [LinkedPeerRecord] = snapshot.linkedPeers
    let pending: [String] = snapshot.pendingOutboundCounterparties
    _ = (peers, pending)
}
