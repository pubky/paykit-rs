package com.synonym.paykit

/**
 * Plain reservation input before crossing the Paykit FFI boundary.
 *
 * Conversion creates native-backed SDK records for calls that require them.
 */
public data class PrivatePaymentListReservationDraft(
    val reservationId: String,
    val identifier: String,
    val payload: String,
    val expiresAt: String? = null,
    val attribution: Map<String, String> = emptyMap(),
) {
    public fun toPaymentEndpointReservation(): PaymentEndpointReservation =
        PaymentEndpointReservation(
            reservationId = reservationId,
            receivingDetail = ReceivingDetail(
                identifier = identifier,
                payload = PaymentPayload(payload),
            ),
            expiresAt = expiresAt,
            attribution = ReservationAttribution(attribution),
        )
}

/**
 * Plain reservation-backed Private Payment List update before FFI conversion.
 */
public data class PrivatePaymentListReservationUpdateDraft(
    val counterparty: String,
    val reservations: List<PrivatePaymentListReservationDraft>,
) {
    public fun toPrivatePaymentListReservationUpdate(): PrivatePaymentListReservationUpdate =
        PrivatePaymentListReservationUpdate(
            counterparty = counterparty,
            reservations = reservations.map { it.toPaymentEndpointReservation() },
        )
}
