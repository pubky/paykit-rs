package com.synonym.paykit.tests

import com.synonym.paykit.*

// The payer's public Payment Endpoints and outbox (Paykit spec 4, Switch spec 11).
internal suspend fun checkPublic(fixture: CheckoutFixture, wallet: NativeWallet) {
    check(wallet.ownPublicEndpoints().isEmpty())
    val endpoint = PublicEndpoint(identifier = "btc-lightning-bolt11", payload = "lnbc1public")

    suspend fun checkSellingScope(publish: suspend () -> PublicationState) {
        try {
            publish()
            error("payer published a public Payment Endpoint")
        } catch (error: WalletException.NeedsUser) {
            error.failure.use { failure ->
                check(failure.code() == "scope_missing")
                checkNotNull(failure.refusal()).use {
                    check((it.kind() as RefusalKind.ScopeMissing).scope == "paykit_request")
                }
            }
        }
    }
    checkSellingScope { wallet.setPublicEndpoint(endpoint.identifier, endpoint.payload) }
    checkSellingScope { wallet.setPublicEndpoints(listOf(endpoint), null) }
    check(wallet.removePublicEndpoint(endpoint.identifier) is PublicationState.NotPublishing)
    check(wallet.setPublicEndpoints(emptyList(), fixture.nowMs()) is PublicationState.NotPublishing)
    check(wallet.updateOwnRecord() is PublicationState.NotPublishing)
    check(wallet.ownPublicEndpoints().isEmpty())
    wallet.flush()
    check(wallet.messageDelivery("00000000-0000-4000-8000-000000000000") == null)
}
