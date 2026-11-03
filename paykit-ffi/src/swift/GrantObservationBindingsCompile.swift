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

// Compile-only consumer fixture; lifecycle admission and confirmation belong to the app.
private func compileGrantLogoutBindingsSurface(
    sdk: PaykitSdkProtocol,
    originalOwner: String,
    access: PubkySessionAccess,
    key: PaykitIdentitySecretKey
) async throws {
    let keyedAccess: PubkySessionAccess = access.withPaykitIdentitySecretKey(key: key)
    let logout: PubkyGrantLogout = try await sdk.captureGrantLogout(expectedPublicKey: originalOwner)
    let binding: PubkyGrantLogoutReceipt = logout.binding()
    let attempt: PubkyGrantLogoutAttempt = try await logout.prepareAttempt()
    let receipt: PubkyGrantLogoutReceipt = try await attempt.execute()
    let acknowledged: PubkyGrantLogoutReceipt? = logout.acknowledged()
    logout.revokeAdmission()
    _ = (binding.publicKey, receipt.grantId, acknowledged, keyedAccess)
}
