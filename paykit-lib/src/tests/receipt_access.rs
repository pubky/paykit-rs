use super::*;

fn receipt_access_json(access: &ReceiptAccess) -> String {
    serde_json::json!({
        "version": access.version,
        "kind": "paykit.receipt_access",
        "reference": access.reference.as_str(),
        "location": &access.location,
        "key": access.key.as_str(),
        "algorithm": &access.algorithm,
    })
    .to_string()
}

#[tokio::test]
async fn issue_receipt_stores_encrypted_receipt_and_sends_access_message() {
    let mut setup = PrivateTestSetup::new().await;
    let reference = PaymentReference::new_v4();
    let draft = ReceiptDraft {
        reference: reference.clone(),
        payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
        amount: Some("1000".to_string()),
        currency: Some("sats".to_string()),
        metadata: HashMap::from([("note".to_string(), "paid".to_string())]),
    };

    let issued = issue_receipt(&setup.sender_session, &mut setup.sender_link, draft)
        .await
        .unwrap();

    assert_eq!(issued.reference, reference);
    assert_eq!(issued.location, ReceiptAccess::location_for(&reference));

    let stored = setup
        .sender_session
        .storage()
        .get(issued.location.clone())
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let receipt = decrypt_receipt(&stored, &issued.key, &issued.location).unwrap();
    assert_eq!(receipt.reference, reference);
    assert_eq!(
        receipt.recipient_public_key,
        setup.sender_link.recipient().clone()
    );
    assert_eq!(receipt.amount.as_deref(), Some("1000"));

    let access = get_receipt_access(&mut setup.receiver_link).await.unwrap();
    assert_eq!(access.len(), 1);
    assert_eq!(access[0].reference, reference);
    assert_eq!(access[0].location, issued.location);
    assert_eq!(access[0].key, issued.key);
}

#[tokio::test]
async fn get_receipt_access_returns_all_available_receipts_in_fifo_order() {
    let mut setup = PrivateTestSetup::new().await;
    let first_reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let second_reference = PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let first_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        location: ReceiptAccess::location_for(&first_reference),
        key: ReceiptDecryptionKey::generate(),
        reference: first_reference.clone(),
        algorithm: "XChaCha20Poly1305".to_string(),
    };
    let second_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        location: ReceiptAccess::location_for(&second_reference),
        key: ReceiptDecryptionKey::generate(),
        reference: second_reference.clone(),
        algorithm: "XChaCha20Poly1305".to_string(),
    };

    let first_json = receipt_access_json(&first_access);
    let second_json = receipt_access_json(&second_access);
    send_raw_private_message(&mut setup.sender_link, &first_json).await;
    send_raw_private_message(&mut setup.sender_link, &second_json).await;

    let received = get_receipt_access(&mut setup.receiver_link).await.unwrap();
    let empty = get_receipt_access(&mut setup.receiver_link).await.unwrap();

    assert_eq!(received.len(), 2);
    assert_eq!(received[0].reference, first_reference);
    assert_eq!(received[1].reference, second_reference);
    assert!(empty.is_empty());
}

#[tokio::test]
async fn get_receipt_access_preserves_valid_receipts_when_one_selected_message_is_malformed() {
    let mut setup = PrivateTestSetup::new().await;
    let first_reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let second_reference = PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let malformed_reference =
        PaymentReference::new("750e8400-e29b-41d4-a716-446655440000").unwrap();
    let first_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        location: ReceiptAccess::location_for(&first_reference),
        key: ReceiptDecryptionKey::generate(),
        reference: first_reference.clone(),
        algorithm: "XChaCha20Poly1305".to_string(),
    };
    let malformed_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        location: ReceiptAccess::location_for(&malformed_reference),
        key: ReceiptDecryptionKey::generate(),
        reference: malformed_reference,
        algorithm: "bad-algorithm".to_string(),
    };
    let second_access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        location: ReceiptAccess::location_for(&second_reference),
        key: ReceiptDecryptionKey::generate(),
        reference: second_reference.clone(),
        algorithm: "XChaCha20Poly1305".to_string(),
    };

    let first_json = receipt_access_json(&first_access);
    let malformed_json = receipt_access_json(&malformed_access);
    let second_json = receipt_access_json(&second_access);
    send_raw_private_message(&mut setup.sender_link, &first_json).await;
    send_raw_private_message(&mut setup.sender_link, &malformed_json).await;
    send_raw_private_message(&mut setup.sender_link, &second_json).await;

    let received = get_receipt_access(&mut setup.receiver_link).await.unwrap();
    let empty = get_receipt_access(&mut setup.receiver_link).await.unwrap();

    assert_eq!(received.len(), 2);
    assert_eq!(received[0].reference, first_reference);
    assert_eq!(received[1].reference, second_reference);
    assert!(empty.is_empty());
}
