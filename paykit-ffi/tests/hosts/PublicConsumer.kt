package com.synonym.paykit.tests

import com.synonym.paykit.*

// The payer's public Payment Endpoints and outbox (Paykit spec 4, Switch spec 11).
internal suspend fun checkPublic(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.ownPublicEndpoints().isEmpty())
    val endpoint = PublicEndpoint(identifier = "btc-lightning-bolt11", payload = "lnbc1public")

    check(wallet.setPublicEndpoint(endpoint.identifier, endpoint.payload) is PublicationState.Published)
    check(wallet.ownPublicEndpoints() == listOf(endpoint))
    check(wallet.removePublicEndpoint(endpoint.identifier) is PublicationState.Published)
    check(wallet.ownPublicEndpoints().isEmpty())
    check(wallet.setPublicEndpoints(emptyList(), fixture.nowMs()) is PublicationState.Published)
    check(wallet.updateOwnRecord() is PublicationState.Published)
    check(wallet.ownPublicEndpoints().isEmpty())
    wallet.flush()
    check(wallet.messageDelivery("00000000-0000-4000-8000-000000000000") == null)
}
