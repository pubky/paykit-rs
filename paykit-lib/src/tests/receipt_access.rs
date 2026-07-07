use super::*;

fn receipt_access_json(access: &ReceiptAccess) -> String {
    let mut object = serde_json::Map::new();
    object.insert("version".to_string(), serde_json::json!(access.version));
    object.insert(
        "kind".to_string(),
        serde_json::json!("paykit.receipt_access"),
    );
    object.insert(
        "event_id".to_string(),
        serde_json::json!(access.event_id.as_str()),
    );
    object.insert(
        "receipt_id".to_string(),
        serde_json::json!(access.receipt_id.as_str()),
    );
    object.insert(
        "payment_reference".to_string(),
        serde_json::json!(access.payment_reference.as_str()),
    );
    if let Some(payment_request_id) = &access.payment_request_id {
        object.insert(
            "payment_request_id".to_string(),
            serde_json::json!(payment_request_id.as_str()),
        );
    }
    if let Some(period) = &access.billing_period {
        object.insert(
            "billing_period".to_string(),
            serde_json::json!({
                "starts_at": period.starts_at,
                "ends_at": period.ends_at,
            }),
        );
    }
    object.insert("location".to_string(), serde_json::json!(&access.location));
    object.insert("key".to_string(), serde_json::json!(access.key.as_str()));
    serde_json::Value::Object(object).to_string()
}

#[tokio::test]
async fn prepared_receipt_can_be_stored_and_sent_in_retryable_steps() {
    let mut setup = PrivateTestSetup::new().await;
    let reference = PaymentReference::new("invoice-2026-0001").unwrap();
    let draft = ReceiptDraft {
        receipt_id: None,
        payment_reference: reference.clone(),
        payment_request_id: None,
        billing_period: None,
        payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
        amount: Some(PaymentAmount::new("2500", "sats").unwrap()),
        metadata: serde_json::json!({
            "settlement_id": "abc-123",
            "details": { "confirmations": 3 }
        })
        .as_object()
        .cloned()
        .unwrap(),
    };

    let prepared = prepare_receipt(&setup.sender_link, &receiver_id(), draft).unwrap();
    assert_eq!(prepared.access.payment_reference, reference);
    assert_eq!(prepared.receipt.receipt_id, prepared.access.receipt_id);
    assert_eq!(
        prepared.access.location,
        ReceiptAccess::location(&receiver_id(), &prepared.access.receipt_id)
    );

    store_prepared_receipt(&setup.sender_session, &prepared)
        .await
        .unwrap();

    let stored = setup
        .sender_session
        .storage()
        .get(prepared.access.location.clone())
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let receipt =
        decrypt_receipt(&stored, &prepared.access.key, &prepared.access.location).unwrap();
    assert_eq!(receipt, prepared.receipt);

    send_receipt_access(&mut setup.sender_link, &prepared.access)
        .await
        .unwrap();
    let access = receive_receipt_access_for_test(&mut setup.receiver_link).await;

    assert_eq!(access, vec![prepared.access]);
}

#[tokio::test]
async fn prepare_receipt_rejects_receiver_mismatch_with_link_scope() {
    let setup = PrivateTestSetup::new().await;
    let draft = ReceiptDraft {
        receipt_id: None,
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
        amount: Some(PaymentAmount::new("2500", "sats").unwrap()),
        metadata: serde_json::Map::new(),
    };

    let result = prepare_receipt(
        &setup.sender_link,
        &PaykitReceiverId::new("tether").unwrap(),
        draft,
    );

    assert!(matches!(result, Err(PaykitError::Validation(_))));
}

#[tokio::test]
async fn send_receipt_access_rejects_receiver_mismatch_with_link_scope() {
    let mut setup = PrivateTestSetup::new().await;
    let draft = ReceiptDraft {
        receipt_id: None,
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
        amount: Some(PaymentAmount::new("2500", "sats").unwrap()),
        metadata: serde_json::Map::new(),
    };
    let prepared = prepare_receipt_for_recipient(
        setup.sender_link.recipient().clone(),
        &PaykitReceiverId::new("tether").unwrap(),
        draft,
    )
    .unwrap();

    let result = send_receipt_access(&mut setup.sender_link, &prepared.access).await;

    assert!(matches!(result, Err(PaykitError::Validation(_))));
}

#[tokio::test]
async fn receipt_access_parser_returns_all_available_receipts_in_fifo_order() {
    let mut setup = PrivateTestSetup::new().await;
    let first_receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let second_receipt_id = ReceiptId::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let first_reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let second_reference = PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let first_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d109").unwrap(),
        receipt_id: first_receipt_id.clone(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location(&receiver_id(), &first_receipt_id),
        key: ReceiptDecryptionKey::generate(),
        payment_reference: first_reference.clone(),
    };
    let second_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d10a").unwrap(),
        receipt_id: second_receipt_id.clone(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location(&receiver_id(), &second_receipt_id),
        key: ReceiptDecryptionKey::generate(),
        payment_reference: second_reference.clone(),
    };

    let first_json = receipt_access_json(&first_access);
    let second_json = receipt_access_json(&second_access);
    send_raw_private_application_message(&mut setup.sender_link, &first_json).await;
    send_raw_private_application_message(&mut setup.sender_link, &second_json).await;

    let received = receive_receipt_access_for_test(&mut setup.receiver_link).await;
    let empty = receive_receipt_access_for_test(&mut setup.receiver_link).await;

    assert_eq!(received.len(), 2);
    assert_eq!(received[0].receipt_id, first_receipt_id);
    assert_eq!(received[0].payment_reference, first_reference);
    assert_eq!(received[1].receipt_id, second_receipt_id);
    assert_eq!(received[1].payment_reference, second_reference);
    assert!(empty.is_empty());
}

#[tokio::test]
async fn receipt_access_parser_preserves_valid_receipts_when_one_message_is_malformed() {
    let mut setup = PrivateTestSetup::new().await;
    let first_receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let second_receipt_id = ReceiptId::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let malformed_receipt_id = ReceiptId::new("750e8400-e29b-41d4-a716-446655440000").unwrap();
    let first_reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let second_reference = PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let malformed_reference =
        PaymentReference::new("750e8400-e29b-41d4-a716-446655440000").unwrap();
    let first_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d10b").unwrap(),
        receipt_id: first_receipt_id.clone(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location(&receiver_id(), &first_receipt_id),
        key: ReceiptDecryptionKey::generate(),
        payment_reference: first_reference.clone(),
    };
    let malformed_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d10c").unwrap(),
        receipt_id: malformed_receipt_id,
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location(&receiver_id(), &ReceiptId::new_v4()),
        key: ReceiptDecryptionKey::generate(),
        payment_reference: malformed_reference,
    };
    let second_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d10d").unwrap(),
        receipt_id: second_receipt_id.clone(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location(&receiver_id(), &second_receipt_id),
        key: ReceiptDecryptionKey::generate(),
        payment_reference: second_reference.clone(),
    };

    let first_json = receipt_access_json(&first_access);
    let malformed_json = receipt_access_json(&malformed_access);
    let second_json = receipt_access_json(&second_access);
    send_raw_private_application_message(&mut setup.sender_link, &first_json).await;
    send_raw_private_application_message(&mut setup.sender_link, &malformed_json).await;
    send_raw_private_application_message(&mut setup.sender_link, &second_json).await;

    let received = receive_receipt_access_for_test(&mut setup.receiver_link).await;
    let empty = receive_receipt_access_for_test(&mut setup.receiver_link).await;

    assert_eq!(received.len(), 2);
    assert_eq!(received[0].receipt_id, first_receipt_id);
    assert_eq!(received[0].payment_reference, first_reference);
    assert_eq!(received[1].receipt_id, second_receipt_id);
    assert_eq!(received[1].payment_reference, second_reference);
    assert!(empty.is_empty());
}
