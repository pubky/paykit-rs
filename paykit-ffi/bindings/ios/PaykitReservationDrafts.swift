import Foundation

/// Plain reservation input before crossing the Paykit FFI boundary.
///
/// Conversion creates native-backed SDK records for calls that require them.
public struct PrivatePaymentListReservationDraft: Equatable, Hashable, Sendable {
    public var reservationId: String
    public var identifier: String
    public var payload: String
    public var expiresAt: String?
    public var attribution: [String: String]

    public init(
        reservationId: String,
        identifier: String,
        payload: String,
        expiresAt: String? = nil,
        attribution: [String: String] = [:]
    ) {
        self.reservationId = reservationId
        self.identifier = identifier
        self.payload = payload
        self.expiresAt = expiresAt
        self.attribution = attribution
    }

    public func toPaymentEndpointReservation() -> PaymentEndpointReservation {
        PaymentEndpointReservation(
            reservationId: reservationId,
            receivingDetail: ReceivingDetail(
                identifier: identifier,
                payload: PaymentPayload(text: payload)
            ),
            expiresAt: expiresAt,
            attribution: ReservationAttribution(fields: attribution)
        )
    }
}

/// Plain reservation-backed Private Payment List update before FFI conversion.
public struct PrivatePaymentListReservationUpdateDraft: Equatable, Hashable, Sendable {
    public var counterparty: String
    public var reservations: [PrivatePaymentListReservationDraft]

    public init(
        counterparty: String,
        reservations: [PrivatePaymentListReservationDraft]
    ) {
        self.counterparty = counterparty
        self.reservations = reservations
    }

    public func toPrivatePaymentListReservationUpdate() -> PrivatePaymentListReservationUpdate {
        PrivatePaymentListReservationUpdate(
            counterparty: counterparty,
            reservations: reservations.map { $0.toPaymentEndpointReservation() }
        )
    }
}
