use std::collections::HashMap;

use chrono::{TimeZone, Utc};

use super::*;
use crate::outbound_private::{
    claim_next_outbound_private_message, mark_outbound_failed, mark_outbound_invalid,
    mark_outbound_sent, queued_outbound_private_messages,
};
use crate::{
    LinkedPeerState, OutboundPrivateMessageStatus, PrivateStreamParseStatus, PublicationStatus,
};

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
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
    NewOutboundPrivateMessage::new(
        counterparty,
        "paykit.private_payment_list".into(),
        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.into(),
        timestamp(),
    )
}

fn outbound_payment_request_message(counterparty: PubkyPublicKey) -> NewOutboundPrivateMessage {
    NewOutboundPrivateMessage::new(
            counterparty,
            "paykit.payment_request".into(),
            r#"{"version":1,"kind":"paykit.payment_request","event_id":"650e8400-e29b-41d4-a716-446655440000","payment_request_id":"550e8400-e29b-41d4-a716-446655440000","request":{"payment_request_id":"550e8400-e29b-41d4-a716-446655440000","terms":{"amount":{"value":"1","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}}"#.into(),
            timestamp(),
        )
}

fn receipt_access_record(counterparty: PubkyPublicKey) -> ReceiptAccessRecord {
    ReceiptAccessRecord {
        counterparty,
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
    PaymentEndpointReservationRecord {
        reservation_id: "reservation-1".into(),
        counterparty,
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
    let identity_public_key = counterparty();
    let saved_identity = crate::IdentityState {
        public_key: Some(identity_public_key.clone()),
        capability: crate::PubkyIdentityCapability::PrivateLinkCapable,
        local_secret_available: true,
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
    assert_eq!(loaded.unwrap().public_key, Some(identity_public_key));
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
    let stream = PrivateStreamItemRecord::from_new(
        0,
        NewPrivateStreamItem::new(NewPrivateStreamItemDetails {
            counterparty: stream_counterparty,
            receive_batch_id: 0,
            raw_json: r#"{"key":"secret"}"#.into(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }),
    );
    let receipt_access = receipt_access_record(stream.counterparty.clone());
    let receipt = receipt_record(receipt_access.counterparty.clone());
    let reservation = payment_endpoint_reservation_record(receipt_access.counterparty.clone());
    let public_endpoint = public_endpoint_record("btc-lightning-bolt11");
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
            public_endpoint.identifier.clone(),
            public_endpoint.clone(),
        )]),
        ..StorageState::default()
    };

    let debug = format!(
            "{link_state:?} {outbound:?} {stream:?} {receipt_access:?} {receipt:?} {reservation:?} {public_endpoint:?} {contact:?} {storage_state:?}"
        );
    assert!(debug.contains("<redacted:"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("outbound-secret"));
    assert!(!debug.contains("receipt-secret"));
    assert!(!debug.contains("alice"));
    assert!(!debug.contains(contact_public_key.as_str()));
    assert!(!debug.contains("[1, 2, 3]"));
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
    let counterparty = counterparty();

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
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(claimed.outbound_message_id, second.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].outbound_message_id, second.outbound_message_id);
}

#[tokio::test]
async fn test_private_payment_list_queue_sends_only_latest_state() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

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
    let counterparty = counterparty();

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
    let counterparty = counterparty();

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
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(claimed.outbound_message_id, first.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();
    assert!(queued
        .iter()
        .any(|message| message.outbound_message_id == second.outbound_message_id));
}

#[tokio::test]
async fn test_peer_link_operation_lease_blocks_until_released() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let first = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
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
                tx.release_peer_link_operation(&counterparty, first.lease_id);
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
    let counterparty = counterparty();
    let identity = IdentityState {
        public_key: Some(counterparty.clone()),
        capability: crate::PubkyIdentityCapability::PrivateLinkCapable,
        local_secret_available: true,
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
    let counterparty = counterparty();
    let identity = IdentityState {
        public_key: Some(counterparty.clone()),
        capability: crate::PubkyIdentityCapability::PrivateLinkCapable,
        local_secret_available: true,
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
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                );
                tx.allocate_receive_batch_id();
                tx.insert_outbound_private_message(outbound_private_message(counterparty.clone()));
                tx.insert_private_stream_item(NewPrivateStreamItem::new(
                    NewPrivateStreamItemDetails {
                        counterparty: counterparty.clone(),
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
    let counterparty = counterparty();

    let first = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
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
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
            })
            .await
            .unwrap(),
        Some(second)
    );
}

#[tokio::test]
async fn test_peer_link_operation_stale_release_keeps_newer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let first = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
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
                tx.release_peer_link_operation(&counterparty, first.lease_id);
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
            })
            .await
            .unwrap(),
        Some(second)
    );
}

#[tokio::test]
async fn test_stale_peer_link_lease_cannot_overwrite_outbound_status() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

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

    assert!(matches!(stale_result, Err(PaykitSdkError::Policy(_))));
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
    let counterparty = counterparty();

    let result: Result<()> = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
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
