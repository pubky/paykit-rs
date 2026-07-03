use std::sync::Arc;

use chrono::Utc;
use paykit_sdk::{
    AmountRecord, BillingPeriodRecord, ReceiptAccessView, ReceiptIssuanceStatus,
    ReceiptIssuanceView, ReceiptRecord, ReceiptRetrievalStatus,
};
use serde_json::{Map as JsonMap, Value as JsonValue};

use super::*;

fn public_key() -> paykit_sdk::PubkyPublicKey {
    conversions::parse_public_key("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io".into())
        .unwrap()
}

#[test]
fn test_receipt_draft_parses_inputs() {
    let draft = FfiReceiptDraft {
        receipt_id: Some("550e8400-e29b-41d4-a716-446655440000".into()),
        payment_reference: Arc::new(FfiPaymentReference::new("invoice-1".into()).unwrap()),
        payment_request_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
        billing_period: Some(FfiBillingPeriod {
            starts_at: "2026-06-01T00:00:00Z".into(),
            ends_at: "2026-07-01T00:00:00Z".into(),
        }),
        payment_endpoint_identifier: Some("btc-lightning-bolt11".into()),
        amount: Some(FfiReceiptAmount {
            value: "25.50".into(),
            asset: "usd".into(),
        }),
        metadata: Arc::new(FfiPrivateJsonObject::new(r#"{"order":"123"}"#.into()).unwrap()),
    };

    let parsed = paykit_lib::ReceiptDraft::try_from(draft).unwrap();

    assert_eq!(
        parsed.receipt_id.as_ref().unwrap().as_str(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
    assert_eq!(parsed.payment_reference.as_str(), "invoice-1");
    assert_eq!(parsed.amount.as_ref().unwrap().asset, "usd");
    assert_eq!(
        parsed
            .metadata
            .get("order")
            .and_then(serde_json::Value::as_str),
        Some("123")
    );
}

#[test]
fn test_receipt_draft_rejects_non_object_metadata() {
    let draft = FfiReceiptDraft {
        receipt_id: None,
        payment_reference: Arc::new(FfiPaymentReference::new("invoice-1".into()).unwrap()),
        payment_request_id: None,
        billing_period: None,
        payment_endpoint_identifier: None,
        amount: None,
        metadata: Arc::new(FfiPrivateJsonObject::from_unchecked_text("[]".into())),
    };

    assert!(matches!(
        paykit_lib::ReceiptDraft::try_from(draft),
        Err(PaykitFfiError::Protocol { code, .. }) if code == "validation"
    ));
}

#[test]
fn test_receipt_issuance_view_redacts_reference() {
    let view = ReceiptIssuanceView {
        counterparty: public_key(),
        counterparty_receiver_id: paykit_sdk::PaykitReceiverId::new("bitkit").unwrap(),
        receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        receipt_access_event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        payment_reference: "invoice secret".into(),
        payment_request_id: None,
        billing_period: None,
        payment_endpoint_identifier: Some("btc-lightning-bolt11".into()),
        amount: Some(AmountRecord {
            value: "10".into(),
            asset: "usd".into(),
        }),
        status: ReceiptIssuanceStatus::AccessQueued,
        outbound_message_id: Some(7),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        stored_at: Some(Utc::now()),
        access_queued_at: Some(Utc::now()),
    };

    let ffi = FfiReceiptIssuanceView::from(view);

    assert_eq!(ffi.status, FfiReceiptIssuanceStatus::AccessQueued);
    assert_eq!(ffi.payment_reference.export_text(), "invoice secret");
    assert!(!format!("{:?}", ffi.payment_reference).contains("invoice secret"));
}

#[test]
fn test_receipt_access_view_maps_status_and_period() {
    let view = ReceiptAccessView {
        counterparty: public_key(),
        counterparty_receiver_id: paykit_sdk::PaykitReceiverId::new("bitkit").unwrap(),
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        payment_reference: "invoice secret".into(),
        payment_request_id: Some("750e8400-e29b-41d4-a716-446655440000".into()),
        billing_period: Some(BillingPeriodRecord {
            starts_at: "2026-06-01T00:00:00Z".into(),
            ends_at: "2026-07-01T00:00:00Z".into(),
        }),
        retrieval_status: ReceiptRetrievalStatus::Retrieved,
        retrieval_attempted_at: Some(Utc::now()),
        retrieved_at: Some(Utc::now()),
        received_at: Utc::now(),
    };

    let ffi = FfiReceiptAccessView::from(view);

    assert_eq!(ffi.retrieval_status, FfiReceiptRetrievalStatus::Retrieved);
    assert_eq!(
        ffi.billing_period.as_ref().unwrap().starts_at,
        "2026-06-01T00:00:00Z"
    );
}

#[test]
fn test_receipt_record_serializes_metadata() {
    let mut metadata = JsonMap::new();
    metadata.insert("source".into(), JsonValue::String("test".into()));

    let record = ReceiptRecord {
        issuer: public_key(),
        issuer_receiver_id: paykit_sdk::PaykitReceiverId::new("bitkit").unwrap(),
        receipt_access_event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_access_key_hash: "hash".into(),
        receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        payment_reference: "invoice secret".into(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key: public_key(),
        payment_endpoint_identifier: Some("btc-lightning-bolt11".into()),
        amount: Some(AmountRecord {
            value: "10".into(),
            asset: "usd".into(),
        }),
        metadata,
        location: "/pub/paykit/v0/private/bitkit/receipts/550e8400-e29b-41d4-a716-446655440000"
            .into(),
        retrieved_at: Utc::now(),
    };

    let ffi = FfiReceiptRecord::try_from(record).unwrap();

    assert!(ffi.metadata.export_text().contains("\"source\":\"test\""));
    assert_eq!(ffi.payment_reference.export_text(), "invoice secret");
    assert!(!format!("{:?}", ffi.payment_reference).contains("invoice secret"));
    assert!(!format!("{:?}", ffi.metadata).contains("test"));
}
