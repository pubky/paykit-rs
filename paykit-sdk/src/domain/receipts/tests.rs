use chrono::{TimeZone, Utc};

use super::*;

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn public_key() -> paykit_lib::PublicKey {
    pubky::Keypair::random().public_key()
}

fn receipt_access_record(
    receipt_id: &paykit_lib::ReceiptId,
    key: &ReceiptDecryptionKey,
    payment_reference: &str,
) -> ReceiptAccessRecord {
    ReceiptAccessRecord {
        counterparty: PubkyPublicKey::from_public_key(&public_key()),
        app_id: paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        app_authorized: false,
        stream_item_id: 0,
        receive_batch_id: 0,
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_id: receipt_id.as_str().into(),
        payment_reference: payment_reference.into(),
        payment_request_id: None,
        billing_period: None,
        location: paykit_lib::ReceiptAccess::location_for(receipt_id),
        key: key.as_str().into(),
        retrieval_status: ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    }
}

fn receipt(
    receipt_id: paykit_lib::ReceiptId,
    payment_reference: &str,
    recipient_public_key: paykit_lib::PublicKey,
) -> Receipt {
    Receipt {
        receipt_id,
        payment_reference: paykit_lib::PaymentReference::new(payment_reference).unwrap(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key,
        payment_endpoint_identifier: Some(
            paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        ),
        amount: Some(paykit_lib::PaymentAmount::new("0.001", "btc").unwrap()),
        metadata: serde_json::json!({"settlement_id": "abc-123"})
            .as_object()
            .cloned()
            .unwrap(),
    }
}

#[test]
fn test_receipt_draft_builder_creates_retry_safe_draft() {
    let draft = ReceiptDraftBuilder::new("invoice-2026-0001")
        .unwrap()
        .with_new_receipt_id()
        .with_payment_endpoint_identifier_text("btc-lightning-bolt11")
        .unwrap()
        .with_amount_text("0.001", "btc")
        .unwrap()
        .with_metadata(
            serde_json::json!({"settlement_id": "abc-123"})
                .as_object()
                .cloned()
                .unwrap(),
        )
        .build()
        .unwrap();

    assert!(draft.receipt_id.is_some());
    assert_eq!(draft.payment_reference.as_str(), "invoice-2026-0001");
    assert_eq!(
        draft.payment_endpoint_identifier.unwrap().as_str(),
        "btc-lightning-bolt11"
    );
    assert_eq!(draft.amount.unwrap().asset, "btc");
    assert_eq!(draft.metadata["settlement_id"], "abc-123");
}

#[test]
fn test_receipt_draft_builder_requires_request_id_for_billing_period() {
    let result = ReceiptDraftBuilder::new("invoice-2026-0001")
        .unwrap()
        .with_billing_period(paykit_lib::BillingPeriod {
            starts_at: "2026-06-01T00:00:00Z".into(),
            ends_at: "2026-07-01T00:00:00Z".into(),
        })
        .build();

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_receipt_draft_builder_debug_redacts_sensitive_fields() {
    let builder = ReceiptDraftBuilder::new("invoice-2026-0001")
        .unwrap()
        .with_amount_text("0.001", "btc")
        .unwrap()
        .with_metadata(
            serde_json::json!({"settlement_id": "abc-123"})
                .as_object()
                .cloned()
                .unwrap(),
        );

    let debug = format!("{builder:?}");

    assert!(!debug.contains("invoice-2026-0001"));
    assert!(!debug.contains("0.001"));
    assert!(!debug.contains("abc-123"));
}

#[test]
fn test_receipt_issuance_record_redacts_sensitive_fields() {
    let counterparty_key = public_key();
    let counterparty = PubkyPublicKey::from_public_key(&counterparty_key);
    let prepared = paykit_lib::prepare_receipt_for_recipient(
        counterparty_key,
        paykit_lib::ReceiptDraft {
            receipt_id: Some(
                paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            ),
            payment_reference: paykit_lib::PaymentReference::new("invoice-2026-0001").unwrap(),
            payment_request_id: None,
            billing_period: None,
            payment_endpoint_identifier: Some(
                paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            ),
            amount: Some(paykit_lib::PaymentAmount::new("0.001", "btc").unwrap()),
            metadata: serde_json::Map::new(),
        },
    )
    .unwrap();
    let key = prepared.access.key.as_str().to_owned();
    let encrypted_receipt = prepared.encrypted_receipt.clone();
    let app_id = paykit_lib::PaykitAppId::new("bitkit").unwrap();
    let access_json = paykit_lib::serialize_receipt_access_json(&app_id, &prepared.access).unwrap();
    let record =
        ReceiptIssuanceRecord::from_prepared(counterparty, app_id, prepared, timestamp()).unwrap();

    let debug = format!("{record:?}");

    assert!(!debug.contains("invoice-2026-0001"));
    assert!(!debug.contains(&key));
    assert!(!debug.contains(&encrypted_receipt));
    assert!(!debug.contains(&access_json));
}

#[test]
fn test_decrypt_receipt_record_from_access_validates_and_redacts() {
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let key = ReceiptDecryptionKey::generate();
    let recipient_public_key = public_key();
    let expected_recipient = PubkyPublicKey::from_public_key(&recipient_public_key);
    let receipt = receipt(
        receipt_id.clone(),
        "invoice-2026-0001",
        recipient_public_key,
    );
    let encrypted = receipt.encrypt(&key).unwrap();
    let access = receipt_access_record(&receipt_id, &key, "invoice-2026-0001");

    let record =
        decrypt_receipt_record_from_access(&access, &encrypted, timestamp(), &expected_recipient)
            .unwrap();

    assert_eq!(record.receipt_id, receipt_id.as_str());
    assert!(receipt_record_matches_access(&record, &access));
    assert_eq!(record.payment_reference, "invoice-2026-0001");
    assert_eq!(record.receipt_access_event_id, access.event_id);
    assert_eq!(record.amount.as_ref().unwrap().asset, "btc");
    assert_eq!(record.metadata["settlement_id"], "abc-123");
    let debug = format!("{record:?}");
    assert!(debug.contains("<redacted:1 fields>"));
    assert!(!debug.contains("abc-123"));
    assert!(!debug.contains("invoice-2026-0001"));
    assert!(!debug.contains(&access.location));
}

#[test]
fn test_receipt_record_matches_access_requires_decrypting_key() {
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let key = ReceiptDecryptionKey::generate();
    let recipient_public_key = public_key();
    let expected_recipient = PubkyPublicKey::from_public_key(&recipient_public_key);
    let receipt = receipt(
        receipt_id.clone(),
        "invoice-2026-0001",
        recipient_public_key,
    );
    let encrypted = receipt.encrypt(&key).unwrap();
    let access = receipt_access_record(&receipt_id, &key, "invoice-2026-0001");
    let record =
        decrypt_receipt_record_from_access(&access, &encrypted, timestamp(), &expected_recipient)
            .unwrap();
    let wrong_key_access = receipt_access_record(
        &receipt_id,
        &ReceiptDecryptionKey::generate(),
        "invoice-2026-0001",
    );

    assert!(!receipt_record_matches_access(&record, &wrong_key_access));
}

#[test]
fn test_receipt_record_matches_access_requires_same_app() {
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let key = ReceiptDecryptionKey::generate();
    let recipient_public_key = public_key();
    let expected_recipient = PubkyPublicKey::from_public_key(&recipient_public_key);
    let receipt = receipt(
        receipt_id.clone(),
        "invoice-2026-0001",
        recipient_public_key,
    );
    let encrypted = receipt.encrypt(&key).unwrap();
    let access = receipt_access_record(&receipt_id, &key, "invoice-2026-0001");
    let record =
        decrypt_receipt_record_from_access(&access, &encrypted, timestamp(), &expected_recipient)
            .unwrap();
    let mut other_app_access = access;
    other_app_access.app_id = paykit_lib::PaykitAppId::new("other-app").unwrap();

    assert!(!receipt_record_matches_access(&record, &other_app_access));
}

#[test]
fn test_receipt_access_record_deserializes_pending_retrieval_defaults() {
    let counterparty = PubkyPublicKey::from_public_key(&public_key());
    let value = serde_json::json!({
        "counterparty": counterparty.as_str(),
        "app_id": "bitkit",
        "stream_item_id": 0,
        "receive_batch_id": 0,
        "event_id": "650e8400-e29b-41d4-a716-446655440000",
        "receipt_id": "550e8400-e29b-41d4-a716-446655440000",
        "payment_reference": "invoice-2026-0001",
        "payment_request_id": null,
        "billing_period": null,
        "location": "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000",
        "key": "receipt-secret",
        "received_at": timestamp(),
    });

    let record: ReceiptAccessRecord = serde_json::from_value(value).unwrap();

    assert_eq!(record.retrieval_status, ReceiptRetrievalStatus::Pending);
    assert!(record.retrieval_attempted_at.is_none());
    assert!(record.retrieved_at.is_none());
    assert!(record.last_retrieval_error.is_none());
}

#[test]
fn test_receipt_access_record_error_clears_success_timestamp() {
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let key = ReceiptDecryptionKey::generate();
    let record =
        receipt_access_record(&receipt_id, &key, "invoice-2026-0001").mark_retrieved(timestamp());

    let failed = record.mark_retrieval_error(
        ReceiptRetrievalStatus::Failed,
        timestamp() + chrono::Duration::seconds(1),
        "decryption failed".into(),
    );

    assert_eq!(failed.retrieval_status, ReceiptRetrievalStatus::Failed);
    assert!(failed.retrieved_at.is_none());
    assert_eq!(
        failed.last_retrieval_error.as_deref(),
        Some("decryption failed")
    );
    let debug = format!("{failed:?}");
    assert!(!debug.contains("decryption failed"));
}

#[test]
fn test_receipt_access_view_hides_storage_only_fields() {
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let key = ReceiptDecryptionKey::generate();
    let access = receipt_access_record(&receipt_id, &key, "invoice-2026-0001");

    let view = ReceiptAccessView::from(&access);
    let json = serde_json::to_string(&view).unwrap();

    assert_eq!(view.payment_reference, "invoice-2026-0001");
    assert!(!json.contains(key.as_str()));
    assert!(!json.contains("/pub/paykit/v0/private/receipts"));
    assert!(!format!("{view:?}").contains("invoice-2026-0001"));
}

#[test]
fn test_decrypt_receipt_record_from_access_rejects_mismatch() {
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let key = ReceiptDecryptionKey::generate();
    let recipient_public_key = public_key();
    let expected_recipient = PubkyPublicKey::from_public_key(&recipient_public_key);
    let receipt = receipt(
        receipt_id.clone(),
        "invoice-2026-0002",
        recipient_public_key,
    );
    let encrypted = receipt.encrypt(&key).unwrap();
    let access = receipt_access_record(&receipt_id, &key, "invoice-2026-0001");

    let err =
        decrypt_receipt_record_from_access(&access, &encrypted, timestamp(), &expected_recipient)
            .unwrap_err();

    assert!(
        matches!(err, PaykitSdkError::Protocol { context: message, .. } if message.contains("Payment Reference"))
    );
}

#[test]
fn test_decrypt_receipt_record_from_access_rejects_wrong_recipient() {
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let key = ReceiptDecryptionKey::generate();
    let receipt = receipt(receipt_id.clone(), "invoice-2026-0001", public_key());
    let encrypted = receipt.encrypt(&key).unwrap();
    let access = receipt_access_record(&receipt_id, &key, "invoice-2026-0001");
    let expected_recipient = PubkyPublicKey::from_public_key(&public_key());

    let err =
        decrypt_receipt_record_from_access(&access, &encrypted, timestamp(), &expected_recipient)
            .unwrap_err();

    assert!(
        matches!(err, PaykitSdkError::Protocol { context: message, .. } if message.contains("recipient"))
    );
}

#[test]
fn test_encrypted_receipt_json_from_bytes_redacts_invalid_utf8() {
    let sentinel = "/pub/paykit/v0/private/SENTINEL_DH_PATH/receipts/receipt-1";
    let mut bytes = sentinel.as_bytes().to_vec();
    bytes.push(0xff);

    let err = encrypted_receipt_json_from_bytes(&bytes).unwrap_err();

    assert!(matches!(
        &err,
        PaykitSdkError::Protocol { context, source }
            if context == "encrypted receipt is not valid UTF-8" && source.is_none()
    ));
    let rendered = format!("{err} / {err:?}");
    assert!(
        !rendered.contains(sentinel),
        "fetched receipt body leaked into Display/Debug: {rendered}"
    );
}

#[test]
fn test_store_encrypted_receipt_error_redacts_receipt_location() {
    // Error context crosses the FFI boundary verbatim, so an Encrypted Receipt
    // write failure must not expose its Receipt Location or Receipt ID.
    let location = "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000";

    let err = store_encrypted_receipt_error(location, anyhow::anyhow!("homeserver rejected put"));

    let context = match &err {
        PaykitSdkError::Transport { context, .. } => context.clone(),
        other => panic!("expected Transport error, got {other:?}"),
    };
    assert_eq!(context, "failed to store encrypted receipt");
    assert!(
        !context.contains(location),
        "Receipt Location leaked into error context: {context}"
    );
    assert!(
        !context.contains("/pub/paykit/v0/private/"),
        "private storage path leaked into error context: {context}"
    );
    // Display is what the FFI layer renders across the boundary; it too must stay clean.
    let rendered = err.to_string();
    assert!(
        !rendered.contains(location),
        "Receipt Location leaked into Display: {rendered}"
    );
}

#[test]
fn test_merge_retrieval_error_not_found_does_not_displace_other_errors() {
    // A confirmed 404 for one Receipt Location must not mask a transport
    // failure from another (often newer) location: transport failures signal
    // the receipt may still be retrievable on retry, while `not_found` crosses
    // the FFI as definitive absence.
    let transport = PaykitSdkError::Transport {
        context: "fetch encrypted receipt".into(),
        source: None,
    };

    let kept = merge_retrieval_error(Some(transport), missing_encrypted_receipt_error("/loc"));

    assert!(
        matches!(kept, PaykitSdkError::Transport { .. }),
        "NotFound displaced an earlier transport error: {kept:?}"
    );
}

#[test]
fn test_merge_retrieval_error_keeps_not_found_when_only_failure() {
    let kept = merge_retrieval_error(None, missing_encrypted_receipt_error("/loc"));

    assert!(matches!(kept, PaykitSdkError::NotFound { .. }));
}

#[test]
fn test_merge_retrieval_error_non_not_found_displaces_previous() {
    let not_found = missing_encrypted_receipt_error("/loc");
    let transport = PaykitSdkError::Transport {
        context: "fetch encrypted receipt".into(),
        source: None,
    };

    let kept = merge_retrieval_error(Some(not_found), transport);

    assert!(matches!(kept, PaykitSdkError::Transport { .. }));
}

#[test]
fn test_missing_encrypted_receipt_error_redacts_receipt_location() {
    // Error messages cross the FFI boundary verbatim, so a missing Encrypted
    // Receipt must not expose its Receipt Location or Receipt ID. A confirmed
    // 404/GONE remains `NotFound` for generated clients.
    let location = "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000";

    let err = missing_encrypted_receipt_error(location);

    let context = match &err {
        PaykitSdkError::NotFound { context, .. } => context.clone(),
        other => panic!("expected NotFound error, got {other:?}"),
    };
    assert_eq!(
        context,
        "encrypted receipt was not found at its receipt location"
    );
    assert!(
        !context.contains("/pub/paykit/v0/private/"),
        "private storage path leaked into error context: {context}"
    );
    // Display is what the FFI layer renders across the boundary; it too must stay clean.
    let rendered = err.to_string();
    assert!(
        !rendered.contains(location),
        "Receipt Location leaked into Display: {rendered}"
    );
}

// CLAUDE.md contract: public reads treat 404/GONE as absence, never as errors.
fn server_error(status: StatusCode) -> PubkyError {
    PubkyError::Request(RequestError::Server {
        status,
        message: "test response".into(),
    })
}

#[test]
fn test_is_not_found_matches_not_found_and_gone() {
    assert!(is_not_found(&server_error(StatusCode::NOT_FOUND)));
    assert!(is_not_found(&server_error(StatusCode::GONE)));
}

#[test]
fn test_is_not_found_rejects_other_statuses_and_variants() {
    assert!(!is_not_found(&server_error(
        StatusCode::INTERNAL_SERVER_ERROR
    )));
    assert!(!is_not_found(&server_error(StatusCode::FORBIDDEN)));

    let validation_error = PubkyError::Request(RequestError::Validation {
        message: "invalid request".into(),
    });
    assert!(!is_not_found(&validation_error));
}
