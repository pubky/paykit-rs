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

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn random_public_key() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn receiver_noise_public_key() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::from_secret(&[7; 32]).public_key())
}

fn receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

fn other_receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/server").unwrap()
}

fn public_endpoint_record(identifier: &str) -> PublicEndpointRecord {
    PublicEndpointRecord {
        identifier: identifier.into(),
        payload: Some("public-endpoint-secret".into()),
        status: PublicationStatus::Published,
        updated_at: timestamp(),
        last_error: None,
    }
}

fn outbound_private_message(counterparty: PubkyPublicKey) -> NewOutboundPrivateMessage {
    outbound_private_message_with_receiver(counterparty, receiver_path())
}

fn outbound_private_message_with_receiver(
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
) -> NewOutboundPrivateMessage {
    NewOutboundPrivateMessage::new(
        counterparty,
        counterparty_receiver_path,
        "paykit.private_payment_list".into(),
        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.into(),
        timestamp(),
    )
}

fn outbound_payment_request_message(counterparty: PubkyPublicKey) -> NewOutboundPrivateMessage {
    NewOutboundPrivateMessage::new(
            counterparty,
            receiver_path(),
            "paykit.payment_request".into(),
            r#"{"version":1,"kind":"paykit.payment_request","event_id":"650e8400-e29b-41d4-a716-446655440000","payment_request_id":"550e8400-e29b-41d4-a716-446655440000","request":{"payment_request_id":"550e8400-e29b-41d4-a716-446655440000","terms":{"amount":{"value":"1","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}}"#.into(),
            timestamp(),
        )
}

fn receipt_access_record(counterparty: PubkyPublicKey) -> ReceiptAccessRecord {
    ReceiptAccessRecord {
        counterparty,
        counterparty_receiver_path: receiver_path(),
        stream_item_id: 0,
        receive_batch_id: 0,
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: None,
        billing_period: None,
        location:
            "/pub/paykit/v0/private/bitkit/wallet/receipts/550e8400-e29b-41d4-a716-446655440000"
                .into(),
        key: "receipt-secret".into(),
        retrieval_status: crate::ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    }
}

fn receipt_record(issuer: PubkyPublicKey) -> ReceiptRecord {
    receipt_record_with_receiver(issuer, receiver_path())
}

fn receipt_record_with_receiver(
    issuer: PubkyPublicKey,
    issuer_receiver_path: PaykitReceiverPath,
) -> ReceiptRecord {
    ReceiptRecord {
        issuer,
        issuer_receiver_path,
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
        location:
            "/pub/paykit/v0/private/bitkit/wallet/receipts/550e8400-e29b-41d4-a716-446655440000"
                .into(),
        retrieved_at: timestamp(),
    }
}

fn payment_endpoint_reservation_record(
    counterparty: PubkyPublicKey,
) -> PaymentEndpointReservationRecord {
    payment_endpoint_reservation_record_with_receiver(counterparty, receiver_path())
}

fn payment_endpoint_reservation_record_with_receiver(
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
) -> PaymentEndpointReservationRecord {
    PaymentEndpointReservationRecord {
        reservation_id: "reservation-1".into(),
        counterparty,
        counterparty_receiver_path,
        identifier: "btc-lightning-bolt11".into(),
        payload_hash: "reserved-payload-hash".into(),
        outbound_message_id: 7,
        attribution: HashMap::from([("contact".into(), "alice".into())]),
        expires_at: None,
        cancellation_started_at: None,
        created_at: timestamp(),
    }
}

#[tokio::test]
async fn test_storage_adapter_supports_erased_transactions() {
    let storage: std::sync::Arc<dyn StorageAdapter> = std::sync::Arc::new(InMemoryStorage::new());
    let local_pubky_public_key = random_public_key();
    let saved_identity = crate::IdentityState {
        local_pubky_public_key: Some(local_pubky_public_key.clone()),
        local_receiver_noise_public_key: Some(receiver_noise_public_key()),
        initialized_at: timestamp(),
        sign_out_generation: 0,
    };
    let value = storage
        .transaction_erased(Box::new(move |tx| {
            tx.save_identity_state(saved_identity);
            Ok(Box::new(42_u32) as Box<dyn std::any::Any + Send>)
        }))
        .await
        .unwrap();

    assert_eq!(*value.downcast::<u32>().unwrap(), 42);
    let loaded = storage
        .transaction_erased(Box::new(|tx| {
            Ok(Box::new(tx.load_identity_state()) as Box<dyn std::any::Any + Send>)
        }))
        .await
        .unwrap();
    let loaded = *loaded.downcast::<Option<crate::IdentityState>>().unwrap();
    assert_eq!(
        loaded.unwrap().local_pubky_public_key,
        Some(local_pubky_public_key)
    );
}

#[test]
fn test_sensitive_storage_debug_is_redacted() {
    let stream_counterparty = random_public_key();
    let link_state = EncryptedLinkStateRecord {
        counterparty: stream_counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
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
        counterparty_receiver_path: receiver_path(),
        receive_batch_id: 0,
        raw_json: r#"{"key":"secret"}"#.into(),
        parsed_version: Some(1),
        parsed_kind: Some("paykit.receipt_access".into()),
        known_paykit_kind: Some("paykit.receipt_access".into()),
        parse_status: PrivateStreamParseStatus::Valid,
        parse_error: Some("parse-error-secret".into()),
        received_at: timestamp(),
    });
    let stream = PrivateStreamItemRecord::from_new(0, new_stream.clone());
    let classification_update = PrivateStreamItemClassificationUpdate {
        stream_item_id: 0,
        parsed_version: Some(1),
        parsed_kind: Some("paykit.kind-secret".into()),
        known_paykit_kind: None,
        parse_status: PrivateStreamParseStatus::UnknownKind,
        parse_error: Some("parse-error-secret".into()),
    };
    let receipt_access = receipt_access_record(stream.counterparty.clone());
    let receipt = receipt_record(receipt_access.counterparty.clone());
    let reservation = payment_endpoint_reservation_record(receipt_access.counterparty.clone());
    let mut public_endpoint = public_endpoint_record("btc-lightning-bolt11");
    public_endpoint.last_error = Some("endpoint-error-secret".into());
    let linked_peer = LinkedPeerRecord {
        counterparty: stream_counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
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
    let contact_public_key = random_public_key();
    let contact = ContactRecord {
        public_key: contact_public_key.clone(),
        receiver_paths: vec![receiver_path()],
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
        public_contact_marker_receiver_path: Some(receiver_path()),
        public_contact_published_at: Some(timestamp()),
        public_contact_removed_at: None,
        public_contact_last_error: Some("marker-secret".into()),
    };
    let storage_state = StorageState {
        contact_records: HashMap::from([(contact_public_key.clone(), contact.clone())]),
        public_endpoint_records: HashMap::from([(
            public_endpoint.identifier.clone(),
            public_endpoint.clone(),
        )]),
        ..StorageState::default()
    };

    let debug = format!(
            "{link_state:?} {outbound:?} {new_stream:?} {stream:?} {classification_update:?} {linked_peer:?} {receipt_access:?} {receipt:?} {reservation:?} {public_endpoint:?} {contact:?} {storage_state:?}"
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
    let counterparty = random_public_key();

    let stream_item_id = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
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
                        counterparty_receiver_path: receiver_path(),
                        receive_batch_id: 7,
                        raw_json: r#"{"version":1,"kind":"paykit.test"}"#.into(),
                        parsed_version: Some(1),
                        parsed_kind: Some("paykit.test".into()),
                        known_paykit_kind: None,
                        parse_status: PrivateStreamParseStatus::UnknownKind,
                        parse_error: None,
                        received_at: timestamp(),
                    },
                ));

                tx.save_event_dedup_record(EventDedupRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
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
        snapshot.linked_peers[&(counterparty.clone(), receiver_path())].state,
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
async fn test_receiver_path_scopes_peer_state_and_queue_claims() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();
    let receiver_path = receiver_path();
    let other_receiver_path = other_receiver_path();

    let (wallet_message, server_message) = storage
        .transaction({
            let counterparty = counterparty.clone();
            let receiver_path = receiver_path.clone();
            let other_receiver_path = other_receiver_path.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path.clone(),
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
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path.clone(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 0,
                    checkpointed_at: timestamp(),
                });
                tx.save_payment_endpoint_reservation(
                    payment_endpoint_reservation_record_with_receiver(
                        counterparty.clone(),
                        receiver_path.clone(),
                    ),
                );
                tx.save_receipt_record(receipt_record_with_receiver(
                    counterparty.clone(),
                    receiver_path.clone(),
                ));
                let wallet_message =
                    tx.insert_outbound_private_message(outbound_private_message_with_receiver(
                        counterparty.clone(),
                        receiver_path.clone(),
                    ));
                let server_message =
                    tx.insert_outbound_private_message(outbound_private_message_with_receiver(
                        counterparty.clone(),
                        other_receiver_path.clone(),
                    ));

                assert!(tx
                    .linked_peer(&counterparty, &other_receiver_path)
                    .is_none());
                assert!(tx
                    .encrypted_link_state(&counterparty, &other_receiver_path)
                    .is_none());
                assert!(tx
                    .payment_endpoint_reservation(
                        &counterparty,
                        &other_receiver_path,
                        "reservation-1",
                    )
                    .is_none());
                assert!(tx
                    .receipt_record(
                        &counterparty,
                        &other_receiver_path,
                        "550e8400-e29b-41d4-a716-446655440000",
                    )
                    .is_none());

                Ok((wallet_message, server_message))
            }
        })
        .await
        .unwrap();

    let claimed = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &other_receiver_path,
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap()
    .unwrap();
    let wallet_queue = queued_outbound_private_messages(&storage, &counterparty, &receiver_path)
        .await
        .unwrap();
    let snapshot = storage.snapshot().unwrap();
    let wallet_record = snapshot
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == wallet_message.outbound_message_id)
        .unwrap();

    assert_eq!(
        claimed.outbound_message_id,
        server_message.outbound_message_id
    );
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    assert_eq!(wallet_queue.len(), 1);
    assert_eq!(
        wallet_queue[0].outbound_message_id,
        wallet_message.outbound_message_id
    );
    assert_eq!(wallet_record.status, OutboundPrivateMessageStatus::Pending);
}

#[tokio::test]
async fn test_save_outbound_private_message_rejects_missing_record() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let result: Result<()> = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_outbound_private_message(OutboundPrivateMessageRecord {
                    outbound_message_id: 99,
                    counterparty,
                    counterparty_receiver_path: receiver_path(),
                    kind: "paykit.private_payment_list".into(),
                    raw_json:
                        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
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

#[tokio::test]
async fn test_invalid_outbound_private_message_does_not_block_later_records() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let (first, second) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let first = tx.insert_outbound_private_message(outbound_private_message(
                    counterparty.clone(),
                ));
                let second =
                    tx.insert_outbound_private_message(outbound_private_message(counterparty));
                Ok((first, second))
            }
        })
        .await
        .unwrap();
    storage
        .transaction({
            let invalid =
                mark_outbound_invalid(first, "invalid private message JSON".into(), timestamp());
            move |tx| {
                tx.save_outbound_private_message(invalid)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    let claimed = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &receiver_path(),
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(claimed.outbound_message_id, second.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    let queued = queued_outbound_private_messages(&storage, &counterparty, &receiver_path())
        .await
        .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].outbound_message_id, second.outbound_message_id);
}

#[tokio::test]
async fn test_private_payment_list_queue_sends_only_latest_state() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let (first, second) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let first = tx.insert_outbound_private_message(outbound_private_message(
                    counterparty.clone(),
                ));
                let second =
                    tx.insert_outbound_private_message(outbound_private_message(counterparty));
                Ok((first, second))
            }
        })
        .await
        .unwrap();

    let claimed = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &receiver_path(),
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(claimed.outbound_message_id, second.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    let snapshot = storage.snapshot().unwrap();
    let first = snapshot
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == first.outbound_message_id)
        .unwrap();
    assert_eq!(first.status, OutboundPrivateMessageStatus::Superseded);
}

#[tokio::test]
async fn test_private_payment_list_queue_reclaims_stale_sending_before_newer_list() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let (first, second) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut first = tx.insert_outbound_private_message(outbound_private_message(
                    counterparty.clone(),
                ));
                first.status = OutboundPrivateMessageStatus::Sending;
                first.last_attempt_at = Some(timestamp() - chrono::Duration::seconds(120));
                tx.save_outbound_private_message(first.clone())?;
                let second =
                    tx.insert_outbound_private_message(outbound_private_message(counterparty));
                Ok((first, second))
            }
        })
        .await
        .unwrap();

    let claimed = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &receiver_path(),
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap();

    let claimed = claimed.unwrap();
    assert_eq!(claimed.outbound_message_id, first.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    assert_eq!(claimed.attempt_count, 1);
    let snapshot = storage.snapshot().unwrap();
    let first = snapshot
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == first.outbound_message_id)
        .unwrap();
    assert_eq!(first.status, OutboundPrivateMessageStatus::Sending);
    let second = snapshot
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == second.outbound_message_id)
        .unwrap();
    assert_eq!(second.status, OutboundPrivateMessageStatus::Pending);
}

#[tokio::test]
async fn test_event_message_queue_preserves_fifo() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let (first, second) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let first = tx.insert_outbound_private_message(outbound_payment_request_message(
                    counterparty.clone(),
                ));
                let second = tx.insert_outbound_private_message(outbound_payment_request_message(
                    counterparty,
                ));
                Ok((first, second))
            }
        })
        .await
        .unwrap();

    let claimed = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &receiver_path(),
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(claimed.outbound_message_id, first.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    let queued = queued_outbound_private_messages(&storage, &counterparty, &receiver_path())
        .await
        .unwrap();
    assert!(queued
        .iter()
        .any(|message| message.outbound_message_id == second.outbound_message_id));
}

#[tokio::test]
async fn test_peer_link_operation_lease_blocks_until_released() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let first = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_path(),
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                ))
            }
        })
        .await
        .unwrap()
        .unwrap();
    let blocked = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_path(),
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                ))
            }
        })
        .await
        .unwrap();
    assert!(blocked.is_none());

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.release_peer_link_operation(&counterparty, &receiver_path(), first.lease_id);
                Ok(())
            }
        })
        .await
        .unwrap();
    let second = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_path(),
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                ))
            }
        })
        .await
        .unwrap();

    assert!(second.is_some());
}

#[tokio::test]
async fn test_clear_identity_scoped_state_preserves_identity_only() {
    let storage = InMemoryStorage::new();
    let local_public_key = random_public_key();
    let counterparty = random_public_key();
    let identity = IdentityState {
        local_pubky_public_key: Some(local_public_key),
        local_receiver_noise_public_key: Some(receiver_noise_public_key()),
        initialized_at: timestamp(),
        sign_out_generation: 1,
    };

    storage
        .transaction({
            let counterparty = counterparty.clone();
            let identity = identity.clone();
            move |tx| {
                tx.save_identity_state(identity);
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(timestamp()),
                    last_private_receive_at: None,
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
                tx.insert_private_stream_item(NewPrivateStreamItem::new(
                    NewPrivateStreamItemDetails {
                        counterparty: counterparty.clone(),
                        counterparty_receiver_path: receiver_path(),
                        receive_batch_id: 0,
                        raw_json: r#"{"version":1,"kind":"paykit.test"}"#.into(),
                        parsed_version: Some(1),
                        parsed_kind: Some("paykit.test".into()),
                        known_paykit_kind: None,
                        parse_status: PrivateStreamParseStatus::UnknownKind,
                        parse_error: None,
                        received_at: timestamp(),
                    },
                ));
                tx.save_receipt_access_record(receipt_access_record(counterparty.clone()));
                tx.save_receipt_record(receipt_record(counterparty));
                tx.clear_identity_scoped_state();
                Ok(())
            }
        })
        .await
        .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.identity_state, Some(identity));
    assert!(snapshot.linked_peers.is_empty());
    assert!(snapshot.public_endpoint_records.is_empty());
    assert!(snapshot.payment_endpoint_reservations.is_empty());
    assert!(snapshot.encrypted_link_states.is_empty());
    assert!(snapshot.outbound_private_messages.is_empty());
    assert!(snapshot.private_stream_items.is_empty());
    assert!(snapshot.event_dedup_records.is_empty());
    assert!(snapshot.receipt_access_records.is_empty());
    assert!(snapshot.receipt_records.is_empty());
}

#[tokio::test]
async fn test_clear_private_identity_scoped_state_preserves_public_endpoints() {
    let storage = InMemoryStorage::new();
    let local_public_key = random_public_key();
    let counterparty = random_public_key();
    let identity = IdentityState {
        local_pubky_public_key: Some(local_public_key),
        local_receiver_noise_public_key: Some(receiver_noise_public_key()),
        initialized_at: timestamp(),
        sign_out_generation: 1,
    };

    storage
        .transaction({
            let counterparty = counterparty.clone();
            let identity = identity.clone();
            move |tx| {
                tx.save_identity_state(identity);
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(timestamp()),
                    last_private_receive_at: None,
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
                tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_path(),
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                );
                tx.allocate_receive_batch_id();
                tx.insert_outbound_private_message(outbound_private_message(counterparty.clone()));
                tx.insert_private_stream_item(NewPrivateStreamItem::new(
                    NewPrivateStreamItemDetails {
                        counterparty: counterparty.clone(),
                        counterparty_receiver_path: receiver_path(),
                        receive_batch_id: 0,
                        raw_json: r#"{"version":1,"kind":"paykit.test"}"#.into(),
                        parsed_version: Some(1),
                        parsed_kind: Some("paykit.test".into()),
                        known_paykit_kind: None,
                        parse_status: PrivateStreamParseStatus::UnknownKind,
                        parse_error: None,
                        received_at: timestamp(),
                    },
                ));
                tx.save_receipt_access_record(receipt_access_record(counterparty.clone()));
                tx.save_receipt_record(receipt_record(counterparty));
                tx.clear_private_identity_scoped_state();
                Ok(())
            }
        })
        .await
        .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.identity_state, Some(identity));
    assert_eq!(snapshot.public_endpoint_records.len(), 1);
    assert!(snapshot.payment_endpoint_reservations.is_empty());
    assert!(snapshot.linked_peers.is_empty());
    assert!(snapshot.encrypted_link_states.is_empty());
    assert!(snapshot.outbound_private_messages.is_empty());
    assert!(snapshot.private_stream_items.is_empty());
    assert!(snapshot.event_dedup_records.is_empty());
    assert!(snapshot.receipt_access_records.is_empty());
    assert!(snapshot.receipt_records.is_empty());
    assert_eq!(snapshot.next_peer_link_operation_lease_id, 1);
    assert_eq!(snapshot.next_outbound_private_message_id, 1);
    assert_eq!(snapshot.next_receive_batch_id, 1);
    assert_eq!(snapshot.next_private_stream_item_id, 1);
}

#[tokio::test]
async fn test_peer_link_operation_lease_can_be_reclaimed_after_expiry() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let first = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_path(),
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(10),
                ))
            }
        })
        .await
        .unwrap()
        .unwrap();
    let second = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_path(),
                    timestamp() + chrono::Duration::seconds(11),
                    timestamp() + chrono::Duration::seconds(71),
                ))
            }
        })
        .await
        .unwrap()
        .unwrap();

    assert_ne!(first.lease_id, second.lease_id);
    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_path()))
            })
            .await
            .unwrap(),
        Some(second)
    );
}

#[tokio::test]
async fn test_peer_link_operation_stale_release_keeps_newer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let first = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_path(),
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(10),
                ))
            }
        })
        .await
        .unwrap()
        .unwrap();
    let second = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_path(),
                    timestamp() + chrono::Duration::seconds(11),
                    timestamp() + chrono::Duration::seconds(71),
                ))
            }
        })
        .await
        .unwrap()
        .unwrap();

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.release_peer_link_operation(&counterparty, &receiver_path(), first.lease_id);
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_path()))
            })
            .await
            .unwrap(),
        Some(second)
    );
}

#[tokio::test]
async fn test_stale_peer_link_lease_cannot_overwrite_outbound_status() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let (record, first_lease) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let record = tx.insert_outbound_private_message(outbound_private_message(
                    counterparty.clone(),
                ));
                let lease = tx
                    .claim_peer_link_operation(
                        &counterparty,
                        &receiver_path(),
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(10),
                    )
                    .unwrap();
                Ok((record, lease))
            }
        })
        .await
        .unwrap();
    let active_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        &receiver_path(),
                        timestamp() + chrono::Duration::seconds(11),
                        timestamp() + chrono::Duration::seconds(71),
                    )
                    .unwrap())
            }
        })
        .await
        .unwrap();
    let sent = mark_outbound_sent(record.clone(), timestamp() + chrono::Duration::seconds(12));
    storage
        .transaction({
            let sent = sent.clone();
            let active_lease = active_lease.clone();
            move |tx| {
                require_peer_link_operation_lease(tx, &active_lease)?;
                tx.save_outbound_private_message(sent)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    let failed = mark_outbound_failed(
        record,
        "late failed send".into(),
        timestamp() + chrono::Duration::seconds(13),
    );
    let stale_result: Result<()> = storage
        .transaction({
            let failed = failed.clone();
            move |tx| {
                require_peer_link_operation_lease(tx, &first_lease)?;
                tx.save_outbound_private_message(failed)?;
                Ok(())
            }
        })
        .await;

    assert!(matches!(stale_result, Err(PaykitSdkError::Policy { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(
        snapshot.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::Sent
    );
    assert!(snapshot.outbound_private_messages[0].last_error.is_none());
}

#[tokio::test]
async fn test_transaction_rolls_back_on_error() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let result: Result<()> = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(timestamp()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });

                Err(PaykitSdkError::Storage {
                    context: "forced rollback".into(),
                    source: None,
                })
            }
        })
        .await;

    assert!(result.is_err());
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.linked_peers.is_empty());
}

#[tokio::test]
async fn test_update_private_stream_item_classification_updates_only_derived_fields() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let stream_item_id = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.insert_private_stream_item(NewPrivateStreamItem::new(
                    NewPrivateStreamItemDetails {
                        counterparty,
                        counterparty_receiver_path: receiver_path(),
                        receive_batch_id: 7,
                        raw_json: r#"{"version":1,"kind":"paykit.receipt_access"}"#.into(),
                        parsed_version: Some(1),
                        parsed_kind: Some("paykit.test".into()),
                        known_paykit_kind: None,
                        parse_status: PrivateStreamParseStatus::UnknownKind,
                        parse_error: Some("stale-parse-error".into()),
                        received_at: timestamp(),
                    },
                )))
            }
        })
        .await
        .unwrap();

    let before = storage.snapshot().unwrap().private_stream_items[0].clone();
    storage
        .transaction(move |tx| {
            tx.update_private_stream_item_classification(PrivateStreamItemClassificationUpdate {
                stream_item_id,
                parsed_version: Some(2),
                parsed_kind: Some("paykit.receipt_access".into()),
                known_paykit_kind: Some("paykit.receipt_access".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
            })
        })
        .await
        .unwrap();

    let after = storage.snapshot().unwrap().private_stream_items[0].clone();
    assert_eq!(after.stream_item_id, before.stream_item_id);
    assert_eq!(after.counterparty, before.counterparty);
    assert_eq!(
        after.counterparty_receiver_path,
        before.counterparty_receiver_path
    );
    assert_eq!(after.receive_batch_id, before.receive_batch_id);
    assert_eq!(after.raw_json, before.raw_json);
    assert_eq!(after.received_at, before.received_at);
    assert_eq!(after.parsed_version, Some(2));
    assert_eq!(after.parsed_kind.as_deref(), Some("paykit.receipt_access"));
    assert_eq!(
        after.known_paykit_kind.as_deref(),
        Some("paykit.receipt_access")
    );
    assert_eq!(after.parse_status, PrivateStreamParseStatus::Valid);
    assert_eq!(after.parse_error, None);
}

#[tokio::test]
async fn test_update_private_stream_item_classification_unknown_item_errors() {
    let storage = InMemoryStorage::new();

    let result: Result<()> = storage
        .transaction(|tx| {
            tx.update_private_stream_item_classification(PrivateStreamItemClassificationUpdate {
                stream_item_id: 99,
                parsed_version: None,
                parsed_kind: None,
                known_paykit_kind: None,
                parse_status: PrivateStreamParseStatus::InvalidJson,
                parse_error: None,
            })
        })
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Storage { .. })));
    assert!(storage.snapshot().unwrap().private_stream_items.is_empty());
}

#[tokio::test]
async fn test_remove_event_dedup_record_removes_and_returns() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let saved = EventDedupRecord {
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        event_kind: "paykit.receipt_access".into(),
        payload_hash: "hash".into(),
        first_stream_item_id: 0,
        duplicate_stream_item_ids: Vec::new(),
        conflicting_stream_item_ids: Vec::new(),
    };
    let (removed, removed_again) = storage
        .transaction({
            let counterparty = counterparty.clone();
            let saved = saved.clone();
            move |tx| {
                tx.save_event_dedup_record(saved.clone());
                let removed =
                    tx.remove_event_dedup_record(&counterparty, &receiver_path(), &saved.event_id);
                let removed_again =
                    tx.remove_event_dedup_record(&counterparty, &receiver_path(), &saved.event_id);
                Ok((removed, removed_again))
            }
        })
        .await
        .unwrap();

    assert_eq!(removed, Some(saved));
    assert_eq!(removed_again, None);
    assert!(storage.snapshot().unwrap().event_dedup_records.is_empty());
}

#[tokio::test]
async fn test_remove_receipt_access_record_removes_and_returns() {
    let storage = InMemoryStorage::new();
    let counterparty = random_public_key();

    let saved = receipt_access_record(counterparty.clone());
    let (removed, removed_again) = storage
        .transaction({
            let counterparty = counterparty.clone();
            let saved = saved.clone();
            move |tx| {
                tx.save_receipt_access_record(saved.clone());
                let removed = tx.remove_receipt_access_record(
                    &counterparty,
                    &receiver_path(),
                    &saved.event_id,
                );
                let removed_again = tx.remove_receipt_access_record(
                    &counterparty,
                    &receiver_path(),
                    &saved.event_id,
                );
                Ok((removed, removed_again))
            }
        })
        .await
        .unwrap();

    assert_eq!(removed, Some(saved));
    assert_eq!(removed_again, None);
    assert!(storage
        .snapshot()
        .unwrap()
        .receipt_access_records
        .is_empty());
}

#[tokio::test]
async fn test_remove_receipt_record_removes_and_returns() {
    let storage = InMemoryStorage::new();
    let issuer = random_public_key();

    let saved = receipt_record(issuer.clone());
    let (removed, removed_again) = storage
        .transaction({
            let issuer = issuer.clone();
            let saved = saved.clone();
            move |tx| {
                tx.save_receipt_record(saved.clone());
                let removed =
                    tx.remove_receipt_record(&issuer, &receiver_path(), &saved.receipt_id);
                let removed_again =
                    tx.remove_receipt_record(&issuer, &receiver_path(), &saved.receipt_id);
                Ok((removed, removed_again))
            }
        })
        .await
        .unwrap();

    assert_eq!(removed, Some(saved));
    assert_eq!(removed_again, None);
    assert!(storage.snapshot().unwrap().receipt_records.is_empty());
}
