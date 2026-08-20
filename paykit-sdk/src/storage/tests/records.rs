use super::*;
use paykit_lib::PrivateMessageKind;

#[tokio::test]
async fn test_private_message_capabilities_are_enforced_per_app() {
    let storage = InMemoryStorage::new();
    let app_id = app_id();
    storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| {
                tx.save_paykit_app_capabilities(
                    &app_id,
                    paykit_lib::PaykitAppCapabilities {
                        private_payments: false,
                        payment_requests: false,
                        receipts: false,
                        outgoing_payments: true,
                    },
                );
                tx.activate_paykit_app(&app_id);
                Ok(())
            }
        })
        .await
        .unwrap();

    storage
        .transaction(|tx| {
            for kind in [
                PrivateMessageKind::PrivatePaymentList,
                PrivateMessageKind::PaymentRequest,
                PrivateMessageKind::PaymentRequestAcceptance,
                PrivateMessageKind::PaymentRequestRejection,
                PrivateMessageKind::PaymentRequestCancellation,
                PrivateMessageKind::PaymentProof,
                PrivateMessageKind::ReceiptAccess,
            ] {
                assert!(matches!(
                    require_paykit_app_capability(tx, &app_id, kind),
                    Err(PaykitSdkError::Policy { .. })
                ));
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[test]
fn test_sensitive_storage_debug_is_redacted() {
    let stream_counterparty = counterparty();
    let link_state = EncryptedLinkStateRecord {
        counterparty: stream_counterparty.clone(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: Some(vec![4, 5, 6]),
        handshake_role: None,
        generation: 0,
        checkpointed_at: timestamp(),
    };
    let mut outbound = OutboundPrivateMessageRecord::from_new(
        0,
        outbound_private_message(stream_counterparty.clone()),
    );
    outbound.last_error = Some("outbound-secret".into());
    let new_stream = NewPrivateStreamItem::new(NewPrivateStreamItemDetails {
        counterparty: stream_counterparty.clone(),
        receive_batch_id: 0,
        raw_json: r#"{"key":"secret"}"#.into(),
        parsed_version: Some(1),
        parsed_kind: Some("paykit.receipt_access".into()),
        parsed_app_id: Some("bitkit".into()),
        known_paykit_kind: Some("paykit.receipt_access".into()),
        parse_status: PrivateStreamParseStatus::Valid,
        parse_error: Some("parse-error-secret".into()),
        received_at: timestamp(),
    });
    let stream = PrivateStreamItemRecord::from_new(0, new_stream.clone());
    let receipt_access = receipt_access_record(stream.counterparty.clone());
    let receipt = receipt_record(receipt_access.counterparty.clone());
    let reservation = payment_endpoint_reservation_record(receipt_access.counterparty.clone());
    let mut public_endpoint = public_endpoint_record("btc-lightning-bolt11");
    public_endpoint.last_error = Some("endpoint-error-secret".into());
    let linked_peer = LinkedPeerRecord {
        counterparty: stream_counterparty.clone(),
        state: LinkedPeerState::Linked,
        last_sync_at: Some(timestamp()),
        last_private_receive_at: Some(timestamp()),
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        local_recovery_marker_last_error: Some("linked-peer-error-secret".into()),
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
    };
    let contact_public_key = counterparty();
    let contact = ContactRecord {
        public_key: contact_public_key.clone(),
        label: Some("contact-secret".into()),
        profile: Some(crate::PaykitProfile {
            display_name: Some("profile-secret".into()),
            image_uri: None,
            extra: None,
        }),
        profile_fetched_at: Some(timestamp()),
        created_at: timestamp(),
        updated_at: timestamp(),
        public_contact_marker_status: crate::PublicationStatus::Published,
        public_contact_published_at: Some(timestamp()),
        public_contact_removed_at: None,
        public_contact_last_error: Some("marker-secret".into()),
    };
    let storage_state = StorageState {
        contact_records: HashMap::from([(contact_public_key.clone(), contact.clone())]),
        public_endpoint_records: HashMap::from([(
            (
                public_endpoint.app_id.clone(),
                public_endpoint.identifier.clone(),
            ),
            public_endpoint.clone(),
        )]),
        ..StorageState::default()
    };

    let debug = format!(
        "{link_state:?} {outbound:?} {new_stream:?} {stream:?} {linked_peer:?} {receipt_access:?} {receipt:?} {reservation:?} {public_endpoint:?} {contact:?} {storage_state:?}"
    );
    assert!(debug.contains("<redacted:"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("outbound-secret"));
    assert!(!debug.contains("receipt-secret"));
    assert!(!debug.contains("alice"));
    assert!(!debug.contains(contact_public_key.as_str()));
    assert!(!debug.contains("[1, 2, 3]"));
    assert!(!debug.contains("endpoint-error-secret"));
    assert!(!debug.contains("linked-peer-error-secret"));
    assert!(!debug.contains("parse-error-secret"));
}

#[tokio::test]
async fn test_transaction_commits_records() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let stream_item_id = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(timestamp()),
                    last_private_receive_at: Some(timestamp()),
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_public_endpoint_record(public_endpoint_record("btc-lightning-bolt11"));
                tx.save_payment_endpoint_reservation(payment_endpoint_reservation_record(
                    counterparty.clone(),
                ));
                tx.insert_outbound_private_message(outbound_private_message(counterparty.clone()));

                let stream_item_id = tx.insert_private_stream_item(NewPrivateStreamItem::new(
                    NewPrivateStreamItemDetails {
                        counterparty: counterparty.clone(),
                        receive_batch_id: 7,
                        raw_json: r#"{"version":1,"kind":"paykit.test","app_id":"bitkit"}"#.into(),
                        parsed_version: Some(1),
                        parsed_kind: Some("paykit.test".into()),
                        parsed_app_id: Some("bitkit".into()),
                        known_paykit_kind: None,
                        parse_status: PrivateStreamParseStatus::UnknownKind,
                        parse_error: None,
                        received_at: timestamp(),
                    },
                ));

                tx.save_event_dedup_record(EventDedupRecord {
                    counterparty: counterparty.clone(),
                    event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
                    event_kind: "paykit.test".into(),
                    payload_hash: "hash".into(),
                    first_stream_item_id: stream_item_id,
                    duplicate_stream_item_ids: Vec::new(),
                    conflicting_stream_item_ids: Vec::new(),
                });

                tx.save_receipt_access_record(receipt_access_record(counterparty.clone()));
                tx.save_receipt_record(receipt_record(counterparty));

                Ok(stream_item_id)
            }
        })
        .await
        .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(stream_item_id, 0);
    assert_eq!(
        snapshot.linked_peers[&counterparty].state,
        LinkedPeerState::Linked
    );
    assert_eq!(snapshot.public_endpoint_records.len(), 1);
    assert_eq!(snapshot.payment_endpoint_reservations.len(), 1);
    assert_eq!(snapshot.outbound_private_messages.len(), 1);
    assert_eq!(snapshot.private_stream_items.len(), 1);
    assert_eq!(snapshot.event_dedup_records.len(), 1);
    assert_eq!(snapshot.receipt_access_records.len(), 1);
    assert_eq!(snapshot.receipt_records.len(), 1);
    assert_eq!(snapshot.next_private_stream_item_id, 1);
    assert_eq!(snapshot.next_outbound_private_message_id, 1);
}

#[tokio::test]
async fn test_storage_keeps_app_owned_endpoint_and_reservation_records_separate() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_public_endpoint_record(public_endpoint_record_for_app(
                    "bitkit",
                    "btc-lightning-bolt11",
                ));
                tx.save_public_endpoint_record(public_endpoint_record_for_app(
                    "tether",
                    "btc-lightning-bolt11",
                ));
                tx.save_payment_endpoint_reservation(payment_endpoint_reservation_record_for_app(
                    counterparty.clone(),
                    "bitkit",
                ));
                tx.save_payment_endpoint_reservation(payment_endpoint_reservation_record_for_app(
                    counterparty,
                    "tether",
                ));
                Ok(())
            }
        })
        .await
        .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.public_endpoint_records.len(), 2);
    assert_eq!(snapshot.payment_endpoint_reservations.len(), 2);
}

#[tokio::test]
async fn test_save_outbound_private_message_rejects_missing_record() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let result: Result<()> = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_outbound_private_message(OutboundPrivateMessageRecord {
                    outbound_message_id: 99,
                    counterparty,
                    app_id: app_id(),
                    kind: "paykit.private_payment_list".into(),
                    raw_json:
                        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#
                            .into(),
                    status: OutboundPrivateMessageStatus::Pending,
                    attempt_count: 0,
                    created_at: timestamp(),
                    updated_at: timestamp(),
                    last_attempt_at: None,
                    sent_at: None,
                    last_error: None,
                })
            }
        })
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Storage { .. })));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}
