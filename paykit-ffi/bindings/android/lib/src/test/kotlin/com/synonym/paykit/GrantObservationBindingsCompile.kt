package com.synonym.paykit

// Compile-only consumer fixture for original grant observation.
@Suppress("UNUSED_VARIABLE")
internal suspend fun compileGrantObservationBindingsSurface(
    sdk: PaykitSdkInterface,
    originalOwner: String,
    delivery: PrivatePaymentListDeliveryReport,
) {
    val observer: PubkyGrantObserver = sdk.captureGrantObserver(expectedPublicKey = originalOwner)
    val owner: String = observer.publicKey()
    val client: String = observer.clientId()
    val grant: String = observer.grantId()
    val result: PubkyGrantObservation = observer.observe()
    when (result) {
        PubkyGrantObservation.ACTIVE, PubkyGrantObservation.UNKNOWN -> Unit
    }
    val acknowledgements: List<PrivatePaymentListAcknowledgement> = delivery.acknowledged
    for (acknowledgement in acknowledgements) {
        val peer: String = acknowledgement.counterparty
        val message: ULong = acknowledgement.outboundMessageId
    }
}
