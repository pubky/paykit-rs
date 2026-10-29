// Compile-only consumer fixture for original grant observation.
private func compileGrantObservationBindingsSurface(
    sdk: PaykitSdkProtocol,
    originalOwner: String,
    delivery: PrivatePaymentListDeliveryReport
) async throws {
    let observer: PubkyGrantObserver = try await sdk.captureGrantObserver(expectedPublicKey: originalOwner)
    let owner: String = observer.publicKey()
    let client: String = observer.clientId()
    let grant: String = observer.grantId()
    let result: PubkyGrantObservation = await observer.observe()
    switch result {
    case .active, .unknown:
        break
    }
    _ = (owner, client, grant)
    let acknowledgements: [PrivatePaymentListAcknowledgement] = delivery.acknowledged
    for acknowledgement in acknowledgements {
        let peer: String = acknowledgement.counterparty
        let message: UInt64 = acknowledgement.outboundMessageId
        _ = (peer, message)
    }
}
