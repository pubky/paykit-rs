use std::collections::HashMap;

use chrono::{TimeZone, Utc};

use super::*;
use crate::domain::outbound_private::{
    claim_next_outbound_private_message, mark_outbound_failed, mark_outbound_invalid,
    mark_outbound_sent, queued_outbound_private_messages,
};
use crate::{
    LinkedPeerState, OutboundPrivateMessageStatus, PrivateStreamParseStatus, PublicationStatus,
};

mod adapter;
mod leases;
mod queue;
mod records;

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn app_id() -> paykit_lib::PaykitAppId {
    paykit_lib::PaykitAppId::new("bitkit").unwrap()
}

fn registered_storage() -> InMemoryStorage {
    InMemoryStorage::with_registered_apps(
        ["bitkit", "paykit-server"]
            .into_iter()
            .map(|app_id| paykit_lib::PaykitAppId::new(app_id).unwrap()),
    )
}

fn public_endpoint_record(identifier: &str) -> PublicEndpointRecord {
    public_endpoint_record_for_app("bitkit", identifier)
}

fn public_endpoint_record_for_app(app_id: &str, identifier: &str) -> PublicEndpointRecord {
    PublicEndpointRecord {
        app_id: paykit_lib::PaykitAppId::new(app_id).unwrap(),
        identifier: identifier.into(),
        payload: Some("public-endpoint-secret".into()),
        status: PublicationStatus::Published,
        updated_at: timestamp(),
        last_error: None,
    }
}

fn outbound_private_message(counterparty: PubkyPublicKey) -> NewOutboundPrivateMessage {
    outbound_private_message_for_app(counterparty, "bitkit")
}

fn outbound_private_message_for_app(
    counterparty: PubkyPublicKey,
    app_id: &str,
) -> NewOutboundPrivateMessage {
    NewOutboundPrivateMessage::new(
        counterparty,
        paykit_lib::PaykitAppId::new(app_id).unwrap(),
        "paykit.private_payment_list".into(),
        format!(
            r#"{{"version":1,"kind":"paykit.private_payment_list","app_id":"{app_id}","payment_endpoints":{{}}}}"#
        ),
        timestamp(),
    )
}

fn outbound_payment_request_message(counterparty: PubkyPublicKey) -> NewOutboundPrivateMessage {
    outbound_payment_request_message_for_app(counterparty, "bitkit")
}

fn outbound_payment_request_message_for_app(
    counterparty: PubkyPublicKey,
    app_id: &str,
) -> NewOutboundPrivateMessage {
    NewOutboundPrivateMessage::new(
        counterparty,
        paykit_lib::PaykitAppId::new(app_id).unwrap(),
        "paykit.payment_request".into(),
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request","app_id":"{app_id}","event_id":"650e8400-e29b-41d4-a716-446655440000","payment_request_id":"550e8400-e29b-41d4-a716-446655440000","request":{{"amount":{{"value":"1","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"required_app_id":null,"metadata":{{}}}}}}"#
        ),
        timestamp(),
    )
}

fn receipt_access_record(counterparty: PubkyPublicKey) -> ReceiptAccessRecord {
    ReceiptAccessRecord {
        counterparty,
        app_id: app_id(),
        app_authorized: false,
        stream_item_id: 0,
        receive_batch_id: 0,
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: None,
        billing_period: None,
        location: "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000".into(),
        key: "receipt-secret".into(),
        retrieval_status: crate::ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    }
}

fn receipt_record(issuer: PubkyPublicKey) -> ReceiptRecord {
    ReceiptRecord {
        issuer,
        app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        receipt_access_event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_access_key_hash: "sha256:test".into(),
        receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key: PubkyPublicKey::from_public_key(
            &pubky::Keypair::random().public_key(),
        ),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location: "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000".into(),
        retrieved_at: timestamp(),
    }
}

fn payment_endpoint_reservation_record(
    counterparty: PubkyPublicKey,
) -> PaymentEndpointReservationRecord {
    payment_endpoint_reservation_record_for_app(counterparty, "bitkit")
}

fn payment_endpoint_reservation_record_for_app(
    counterparty: PubkyPublicKey,
    app_id: &str,
) -> PaymentEndpointReservationRecord {
    PaymentEndpointReservationRecord {
        reservation_id: "reservation-1".into(),
        counterparty,
        app_id: paykit_lib::PaykitAppId::new(app_id).unwrap(),
        identifier: "btc-lightning-bolt11".into(),
        payload_hash: "reserved-payload-hash".into(),
        outbound_message_id: 7,
        attribution: HashMap::from([("contact".into(), "alice".into())]),
        expires_at: None,
        cancellation_started_at: None,
        created_at: timestamp(),
    }
}
