use chrono::{TimeZone, Utc};

use super::*;
use crate::{
    domain::linked_peers::LinkedPeerState,
    storage::{
        EncryptedLinkStateRecord, InMemoryStorage, LinkedPeerRecord, PaymentRequestExecutionClaim,
    },
    IdentityState, PaykitSdkError, PrivateStreamParseStatus,
};

struct ValidatingStorage(InMemoryStorage);

#[async_trait::async_trait]
impl StorageAdapter for ValidatingStorage {
    async fn transaction_erased<'a>(
        &self,
        f: crate::storage::StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        self.0
            .transaction_erased(Box::new(move |tx| {
                crate::validate_storage_state(&tx.export_storage_state())?;
                let result = f(tx)?;
                crate::validate_storage_state(&tx.export_storage_state())?;
                Ok(result)
            }))
            .await
    }
}

fn validating_storage() -> ValidatingStorage {
    ValidatingStorage(InMemoryStorage::from_state(crate::storage::StorageState {
        identity_state: Some(IdentityState {
            public_key: Some(counterparty()),
            initialized_at: timestamp(),
        }),
        ..Default::default()
    }))
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn private_message(raw_json: &str) -> PrivateApplicationMessage {
    let value: serde_json::Value = serde_json::from_str(raw_json).unwrap();
    PrivateApplicationMessage {
        version: value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u8::try_from(version).ok()),
        kind: value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        app_id: value
            .get("app_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        raw_json: raw_json.to_owned(),
    }
}

fn payment_request_raw(reference: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request","app_id":"bitkit","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"{reference}","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"required_app_id":null,"metadata":{{}}}}}}"#
    )
}

fn payment_request_cancellation_raw() -> &'static str {
    r#"{"version":1,"kind":"paykit.payment_request_cancellation","app_id":"bitkit","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"}"#
}

fn receipt_access_raw(event_id: &str, receipt_id: &str, reference: &str) -> String {
    let receipt_id = paykit_lib::ReceiptId::new(receipt_id).unwrap();
    receipt_access_raw_with_location(
        event_id,
        receipt_id.as_str(),
        reference,
        &paykit_lib::ReceiptAccess::location_for(&receipt_id),
    )
}

fn receipt_access_raw_with_location(
    event_id: &str,
    receipt_id: &str,
    reference: &str,
    location: &str,
) -> String {
    let key = paykit_lib::ReceiptDecryptionKey::generate();
    format!(
        r#"{{"version":1,"kind":"paykit.receipt_access","app_id":"bitkit","event_id":"{event_id}","receipt_id":"{receipt_id}","payment_reference":"{reference}","location":"{location}","key":"{}"}}"#,
        key.as_str()
    )
}

#[test]
fn test_unknown_private_message_requires_valid_app_id() {
    for raw_json in [
        r#"{"version":1,"kind":"paykit.future"}"#,
        r#"{"version":1,"kind":"paykit.future","app_id":"bad/path"}"#,
    ] {
        let classification = classify_private_application_message(&private_message(raw_json));
        assert_eq!(classification.status, PrivateStreamParseStatus::InvalidJson);
        assert!(classification.app_id.is_none());
    }

    let classification = classify_private_application_message(&private_message(
        r#"{"version":1,"kind":"paykit.future","app_id":"bitkit"}"#,
    ));
    assert_eq!(classification.status, PrivateStreamParseStatus::UnknownKind);
    assert_eq!(classification.app_id.unwrap().as_str(), "bitkit");
}

#[tokio::test]
async fn test_persist_private_stream_batch_stores_messages_and_checkpoint() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
                    state: LinkedPeerState::Linked,
                    last_sync_at: None,
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let link_state = EncryptedLinkStateRecord {
        counterparty: counterparty.clone(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: None,
        handshake_role: None,
        generation: 1,
        checkpointed_at: timestamp(),
    };
    let messages = vec![
        private_message(r#"{"version":1,"kind":"paykit.unknown","app_id":"bitkit","body":{}}"#),
        private_message(&payment_request_raw("invoice-2026-0001")),
    ];

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        messages,
        Some(link_state.clone()),
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(report.receive_batch_id, Some(0));
    assert_eq!(report.stream_item_ids, vec![0, 1]);
    assert_eq!(snapshot.private_stream_items.len(), 2);
    assert_eq!(
        snapshot.private_stream_items[0].parse_status,
        PrivateStreamParseStatus::UnknownKind
    );
    assert_eq!(
        snapshot.private_stream_items[1].parse_status,
        PrivateStreamParseStatus::Valid
    );
    assert_eq!(snapshot.encrypted_link_states[&counterparty], link_state);
    assert_eq!(snapshot.event_dedup_records.len(), 1);
    assert_eq!(snapshot.outbound_private_messages.len(), 1);
    let confirmation = paykit_lib::parse_delivery_confirmation_json(
        &snapshot.outbound_private_messages[0].raw_json,
    )
    .unwrap();
    assert_eq!(
        confirmation.event_id().as_str(),
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101"
    );
    assert_eq!(
        confirmation.payload_hash(),
        payload_hash(&payment_request_raw("invoice-2026-0001"))
    );
    let peer = snapshot.linked_peers.get(&counterparty).unwrap();
    assert_eq!(peer.last_private_receive_at, Some(timestamp()));
    assert_eq!(peer.last_sync_at, Some(timestamp()));
}

#[tokio::test]
async fn test_duplicate_event_requeues_lost_confirmation_without_reapplying() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    let raw = payment_request_raw("invoice-1");
    persist_private_stream_batch(
        &storage,
        peer.clone(),
        vec![private_message(&raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    let first = storage
        .transaction(|tx| {
            let mut confirmation = tx.outbound_private_messages(&peer).remove(0);
            confirmation.attempt_count = 1;
            confirmation.last_attempt_at = Some(timestamp());
            let confirmation =
                crate::domain::outbound_private::mark_outbound_sent(confirmation, timestamp());
            tx.save_outbound_private_message(confirmation.clone())?;
            Ok(confirmation)
        })
        .await
        .unwrap();
    let restarted = InMemoryStorage::from_state(storage.snapshot().unwrap());
    persist_private_stream_batch(
        &restarted,
        peer.clone(),
        vec![private_message(&raw), private_message(&raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    let state = restarted.snapshot().unwrap();
    assert_eq!(state.outbound_private_messages.len(), 1);
    assert_eq!(
        state.outbound_private_messages[0].outbound_message_id,
        first.outbound_message_id
    );
    assert_eq!(state.outbound_private_messages[0].raw_json, first.raw_json);
    assert_eq!(
        state.outbound_private_messages[0].status,
        crate::OutboundPrivateMessageStatus::Pending
    );
    assert_eq!(
        state
            .event_dedup_records
            .values()
            .next()
            .unwrap()
            .duplicate_stream_item_ids,
        vec![1, 2]
    );
    restarted
        .transaction(|tx| {
            let requests = payment_request_records_from_transaction(tx, &peer, timestamp())?;
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].last_stream_item_id, Some(0));
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_confirmation_matches_attempted_event_peer_id_and_payload() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    let other_peer = counterparty();
    let raw = payment_request_raw("invoice-1");
    let event_id = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101";
    let app_id = PaykitAppId::new("bitkit").unwrap();
    let outbound = storage
        .transaction(|tx| {
            tx.insert_outbound_private_message(crate::storage::NewOutboundPrivateMessage::new(
                peer.clone(),
                app_id.clone(),
                "paykit.payment_request".into(),
                raw.clone(),
                timestamp(),
            ))
        })
        .await
        .unwrap();
    let confirmation_json = |event_id: &str, payload: &str| {
        paykit_lib::serialize_delivery_confirmation(
            &paykit_lib::DeliveryConfirmation::new(
                app_id.clone(),
                paykit_lib::EventId::new(event_id).unwrap(),
                payload_hash(payload),
            )
            .unwrap(),
        )
        .unwrap()
    };
    let valid = confirmation_json(event_id, &raw);
    // A guessed ID cannot confirm work that has never left the queue.
    persist_private_stream_batch(
        &storage,
        peer.clone(),
        vec![private_message(&valid)],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    assert!(storage.snapshot().unwrap().outbound_private_messages[0]
        .confirmed_at
        .is_none());
    storage
        .transaction(|tx| {
            let mut attempted = outbound.clone();
            attempted.status = crate::OutboundPrivateMessageStatus::Failed;
            attempted.attempt_count = 1;
            attempted.last_attempt_at = Some(timestamp());
            attempted.last_error = Some("ambiguous publication".into());
            attempted.prepared_send = Some(crate::storage::PreparedOutboundPrivateSend {
                destination_path: "/reserved-slot".into(),
                ciphertext: vec![1, 2, 3],
            });
            tx.save_outbound_private_message(attempted)
        })
        .await
        .unwrap();
    for (sender, confirmation) in [
        (other_peer, valid.clone()),
        (
            peer.clone(),
            confirmation_json("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102", &raw),
        ),
        (
            peer.clone(),
            confirmation_json(event_id, &payment_request_raw("different")),
        ),
    ] {
        persist_private_stream_batch(
            &storage,
            sender,
            vec![private_message(&confirmation)],
            None,
            timestamp(),
        )
        .await
        .unwrap();
        assert!(storage.snapshot().unwrap().outbound_private_messages[0]
            .confirmed_at
            .is_none());
    }
    persist_private_stream_batch(
        &storage,
        peer,
        vec![private_message(&valid), private_message(&valid)],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    let state = storage.snapshot().unwrap();
    assert_eq!(
        state.outbound_private_messages.len(),
        1,
        "confirmations do not generate confirmations"
    );
    let confirmed = &state.outbound_private_messages[0];
    assert_eq!(confirmed.confirmed_at, Some(timestamp()));
    assert!(
        confirmed.prepared_send.is_some(),
        "the reserved Noise slot still needs publication"
    );
    assert_eq!(
        confirmed.status,
        crate::OutboundPrivateMessageStatus::Failed
    );
    assert!(state.event_dedup_records.is_empty());
}

#[tokio::test]
async fn test_failed_intake_commit_keeps_event_checkpoint_and_confirmation_atomic() {
    struct RejectCommit(InMemoryStorage);
    #[async_trait::async_trait]
    impl StorageAdapter for RejectCommit {
        async fn transaction_erased<'a>(
            &self,
            f: crate::storage::StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn std::any::Any + Send>> {
            self.0
                .transaction_erased(Box::new(move |tx| {
                    f(tx)?;
                    Err(PaykitSdkError::Storage {
                        context: "commit failed".into(),
                        source: None,
                    })
                }))
                .await
        }
    }
    let storage = RejectCommit(InMemoryStorage::new());
    let peer = counterparty();
    let checkpoint = EncryptedLinkStateRecord {
        counterparty: peer.clone(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: None,
        handshake_role: None,
        generation: 1,
        checkpointed_at: timestamp(),
    };
    let message = private_message(&payment_request_raw("invoice-1"));
    assert!(persist_private_stream_batch(
        &storage,
        peer.clone(),
        vec![message.clone()],
        Some(checkpoint.clone()),
        timestamp()
    )
    .await
    .is_err());
    let state = storage.0.snapshot().unwrap();
    assert!(state.private_stream_items.is_empty());
    assert!(state.event_dedup_records.is_empty());
    assert!(state.outbound_private_messages.is_empty());
    assert!(state.encrypted_link_states.is_empty());
    persist_private_stream_batch(
        &storage.0,
        peer.clone(),
        vec![message],
        Some(checkpoint.clone()),
        timestamp(),
    )
    .await
    .unwrap();
    let state = storage.0.snapshot().unwrap();
    assert_eq!(state.private_stream_items.len(), 1);
    assert_eq!(state.event_dedup_records.len(), 1);
    assert_eq!(state.outbound_private_messages.len(), 1);
    assert_eq!(state.encrypted_link_states[&peer], checkpoint);
}

#[tokio::test]
async fn test_persist_private_stream_batch_reconciles_terminal_execution_claims() {
    let mut malformed: serde_json::Value =
        serde_json::from_str(payment_request_cancellation_raw()).unwrap();
    malformed["reason"] = serde_json::Value::Null;
    for (raw, expected_state) in [
        (
            payment_request_cancellation_raw().to_owned(),
            PaymentRequestLifecycleState::Canceled,
        ),
        (
            payment_request_raw("conflicting-reference"),
            PaymentRequestLifecycleState::InvalidConflict,
        ),
        (
            malformed.to_string(),
            PaymentRequestLifecycleState::InvalidConflict,
        ),
    ] {
        let storage = validating_storage();
        let counterparty = counterparty();
        persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            vec![private_message(&payment_request_raw("invoice-2026-0001"))],
            None,
            timestamp(),
        )
        .await
        .unwrap();
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_payment_request_execution_claim(PaymentRequestExecutionClaim {
                        counterparty,
                        payment_request_id: "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33".into(),
                        app_id: PaykitAppId::new("wallet").unwrap(),
                        claimed_at: timestamp(),
                    });
                    Ok(())
                }
            })
            .await
            .unwrap();

        let checkpoint = EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: 1,
            checkpointed_at: timestamp(),
        };
        let report = persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            vec![private_message(&raw)],
            Some(checkpoint.clone()),
            timestamp(),
        )
        .await
        .unwrap();

        assert_eq!(report.stream_item_ids, vec![1]);
        storage
            .transaction(|tx| {
                assert!(tx
                    .payment_request_execution_claim(
                        &counterparty,
                        "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
                    )
                    .is_none());
                let records =
                    payment_request_records_from_transaction(tx, &counterparty, timestamp())?;
                assert_eq!(records[0].state, expected_state);
                assert_eq!(tx.encrypted_link_state(&counterparty), Some(checkpoint));
                Ok(())
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn test_persist_private_stream_batch_preserves_cached_receipt_conflict_history() {
    struct Offline;

    #[async_trait::async_trait]
    impl crate::PubkySessionProvider for Offline {
        async fn load_session_access(&self) -> Result<Option<crate::PubkySessionAccess>> {
            Ok(None)
        }

        async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
            Ok(None)
        }

        async fn clear_session_access(&self) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::PaymentAdapter for Offline {}

    for conflicting_event_id in [
        "650e8400-e29b-41d4-a716-446655440000",
        "750e8400-e29b-41d4-a716-446655440000",
    ] {
        let storage = validating_storage();
        let counterparty = counterparty();
        let event_id = "650e8400-e29b-41d4-a716-446655440000";
        let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
        let raw = receipt_access_raw(event_id, receipt_id, "invoice-2026-0001");
        persist_private_stream_batch_with_link_lease(
            &storage,
            counterparty.clone(),
            vec![private_message(&raw)],
            None,
            Some(vec![PaykitAppId::new("bitkit").unwrap()]),
            None,
            timestamp(),
        )
        .await
        .unwrap();
        storage
            .transaction(|tx| {
                let mut access = tx
                    .receipt_access_record_by_receipt_id(&counterparty, receipt_id)
                    .unwrap();
                access.retrieval_status = crate::ReceiptRetrievalStatus::Retrieved;
                access.retrieval_attempted_at = Some(timestamp());
                access.retrieved_at = Some(timestamp());
                tx.save_receipt_record(crate::ReceiptRecord {
                    issuer: counterparty.clone(),
                    app_id: access.app_id.clone(),
                    receipt_access_event_id: access.event_id.clone(),
                    receipt_access_key_hash: crate::domain::receipts::receipt_access_key_hash(
                        &access.key,
                    ),
                    receipt_id: receipt_id.into(),
                    payment_reference: access.payment_reference.clone(),
                    payment_request_id: None,
                    billing_period: None,
                    recipient_public_key: tx.load_identity_state().unwrap().public_key.unwrap(),
                    payment_endpoint_identifier: None,
                    amount: None,
                    metadata: Default::default(),
                    location: access.location.clone(),
                    retrieved_at: timestamp(),
                });
                tx.save_receipt_access_record(access);
                Ok(())
            })
            .await
            .unwrap();

        let mut conflicting: serde_json::Value = serde_json::from_str(&raw).unwrap();
        conflicting["event_id"] = conflicting_event_id.into();
        conflicting["payment_reference"] = "conflicting-reference".into();
        let checkpoint = EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: 1,
            checkpointed_at: timestamp(),
        };
        let report = persist_private_stream_batch_with_link_lease(
            &storage,
            counterparty.clone(),
            vec![private_message(&conflicting.to_string())],
            Some(checkpoint.clone()),
            Some(vec![PaykitAppId::new("bitkit").unwrap()]),
            None,
            timestamp(),
        )
        .await
        .unwrap();
        assert_eq!(report.stream_item_ids, vec![1]);
        storage
            .transaction(|tx| {
                assert!(tx.receipt_record(&counterparty, receipt_id).is_some());
                assert_eq!(tx.private_stream_items(&counterparty).len(), 2);
                assert_eq!(tx.encrypted_link_state(&counterparty), Some(checkpoint));
                Ok(())
            })
            .await
            .unwrap();

        let backup = crate::export_backup_state(&storage).await.unwrap();
        let backup = serde_json::from_str(&serde_json::to_string(&backup).unwrap()).unwrap();
        let restored = ValidatingStorage(InMemoryStorage::new());
        crate::backup::restore_backup_state(&restored, backup)
            .await
            .unwrap();
        assert_eq!(restored.0.snapshot().unwrap().receipt_records.len(), 1);
        for storage in [storage, restored] {
            let sdk = crate::PaykitSdk::new(
                storage,
                Offline,
                Offline,
                crate::PaykitSdkConfig::new("wallet").unwrap(),
            );
            assert!(sdk.receipt_records(&counterparty).await.unwrap().is_empty());
            assert!(sdk.receipts_from(&counterparty).await.unwrap().is_empty());
            assert!(sdk.receipts().await.unwrap().is_empty());
            assert!(matches!(
                sdk.retrieve_receipt(counterparty.clone(), receipt_id).await,
                Err(PaykitSdkError::Protocol { .. })
            ));
        }
    }
}

#[tokio::test]
async fn test_persist_private_stream_batch_empty_checkpoint_updates_sync_time() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
                    state: LinkedPeerState::Linked,
                    last_sync_at: None,
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let link_state = EncryptedLinkStateRecord {
        counterparty: counterparty.clone(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: None,
        handshake_role: None,
        generation: 1,
        checkpointed_at: timestamp(),
    };

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        Vec::new(),
        Some(link_state),
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let peer = snapshot.linked_peers.get(&counterparty).unwrap();
    assert!(report.stream_item_ids.is_empty());
    assert_eq!(report.receive_batch_id, None);
    assert_eq!(snapshot.next_receive_batch_id, 0);
    assert_eq!(peer.last_private_receive_at, None);
    assert_eq!(peer.last_sync_at, Some(timestamp()));
}

#[tokio::test]
async fn test_persist_private_stream_batch_indexes_receipt_access() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let raw = receipt_access_raw(event_id, receipt_id, "invoice-2026-0001");

    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![private_message(&raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let records = crate::domain::receipts::receipt_access_records(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].stream_item_id, 0);
    assert_eq!(records[0].receive_batch_id, 0);
    assert_eq!(records[0].event_id, event_id);
    assert_eq!(records[0].receipt_id, receipt_id);
    assert_eq!(records[0].payment_reference, "invoice-2026-0001");
    assert!(records[0].payment_request_id.is_none());
    assert!(records[0].billing_period.is_none());
    assert!(records[0].location.ends_with(receipt_id));
    let debug = format!("{:?}", records[0]);
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains(&records[0].key));

    let indexed = crate::domain::receipts::receipt_access_record_by_receipt_id(
        &storage,
        &counterparty,
        receipt_id,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(indexed.event_id, event_id);
}

#[tokio::test]
async fn test_persist_private_stream_batch_dedupes_receipt_access_index() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let duplicate_raw = receipt_access_raw(event_id, receipt_id, "invoice-2026-0001");
    let conflicting_raw = receipt_access_raw(
        event_id,
        "750e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0002",
    );

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![
            private_message(&duplicate_raw),
            private_message(&duplicate_raw),
            private_message(&conflicting_raw),
        ],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let records = crate::domain::receipts::receipt_access_records(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].stream_item_id, 0);
    assert_eq!(records[0].receipt_id, receipt_id);
    assert_eq!(report.event_conflicts.len(), 1);
    assert_eq!(report.event_conflicts[0].conflicting_stream_item_id, 2);
    let snapshot = storage.snapshot().unwrap();
    let dedupe = snapshot
        .event_dedup_records
        .get(&(counterparty, event_id.into()))
        .unwrap();
    assert_eq!(dedupe.duplicate_stream_item_ids, vec![1]);
    assert_eq!(dedupe.conflicting_stream_item_ids, vec![2]);
}

#[tokio::test]
async fn test_persist_private_stream_batch_skips_malformed_receipt_access_index() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let raw = receipt_access_raw_with_location(
        "650e8400-e29b-41d4-a716-446655440000",
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "/pub/paykit/v0/private/receipts/not-the-receipt-id",
    );

    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![private_message(&raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(
        snapshot.private_stream_items[0].parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
    let records = crate::domain::receipts::receipt_access_records(&storage, &counterparty)
        .await
        .unwrap();
    assert!(records.is_empty());
}

#[tokio::test]
async fn test_persist_private_stream_batch_marks_event_id_conflicts() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let messages = vec![
        private_message(&payment_request_raw("invoice-2026-0001")),
        private_message(&payment_request_raw("invoice-2026-0002")),
    ];

    let report =
        persist_private_stream_batch(&storage, counterparty.clone(), messages, None, timestamp())
            .await
            .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let record = snapshot
        .event_dedup_records
        .get(&(counterparty, "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101".into()))
        .unwrap();
    assert_eq!(report.event_conflicts.len(), 1);
    assert_eq!(record.first_stream_item_id, 0);
    assert_eq!(record.conflicting_stream_item_ids, vec![1]);
}

#[tokio::test]
async fn test_persist_private_stream_batch_scopes_event_dedupe_by_counterparty() {
    let storage = InMemoryStorage::new();
    let first_counterparty = counterparty();
    let second_counterparty = counterparty();

    let first_report = persist_private_stream_batch(
        &storage,
        first_counterparty,
        vec![private_message(&payment_request_raw("invoice-2026-0001"))],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    let second_report = persist_private_stream_batch(
        &storage,
        second_counterparty,
        vec![private_message(&payment_request_raw("invoice-2026-0002"))],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert!(first_report.event_conflicts.is_empty());
    assert!(second_report.event_conflicts.is_empty());
    assert_eq!(snapshot.event_dedup_records.len(), 2);
}

#[tokio::test]
async fn test_persist_private_stream_batch_keeps_malformed_recognized_messages() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let malformed = r#"{"version":1,"kind":"paykit.payment_request","app_id":"bitkit","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"ten","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"required_app_id":null,"metadata":{}}}"#;

    persist_private_stream_batch(
        &storage,
        counterparty,
        vec![private_message(malformed)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let item = &snapshot.private_stream_items[0];
    assert_eq!(
        item.parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
    assert!(item
        .parse_error
        .as_ref()
        .is_some_and(|error| error.contains("amount.value")));
    assert_eq!(snapshot.event_dedup_records.len(), 1);
}

#[tokio::test]
async fn test_persist_private_stream_batch_keeps_invalid_json_payloads() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let raw_json = "not json";

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![PrivateApplicationMessage {
            version: None,
            kind: None,
            app_id: None,
            raw_json: raw_json.into(),
        }],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let item = &snapshot.private_stream_items[0];
    assert_eq!(report.stream_item_ids, vec![0]);
    assert_eq!(item.counterparty, counterparty);
    assert_eq!(item.raw_json, raw_json);
    assert_eq!(item.parse_status, PrivateStreamParseStatus::InvalidJson);
}

#[tokio::test]
async fn test_persist_private_stream_batch_records_invalid_utf8_marker_error() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let raw_json = "paykit.invalid_utf8_private_message:_w";

    persist_private_stream_batch(
        &storage,
        counterparty,
        vec![PrivateApplicationMessage {
            version: None,
            kind: None,
            app_id: None,
            raw_json: raw_json.into(),
        }],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let item = &snapshot.private_stream_items[0];
    assert_eq!(item.parse_status, PrivateStreamParseStatus::InvalidJson);
    assert!(item
        .parse_error
        .as_ref()
        .is_some_and(|error| error.contains("valid UTF-8")));
}

#[tokio::test]
async fn test_persist_private_stream_batch_rolls_back_with_stale_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let first_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(10),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp() + chrono::Duration::seconds(11),
                    timestamp() + chrono::Duration::seconds(71),
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let link_state = EncryptedLinkStateRecord {
        counterparty: counterparty.clone(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: None,
        handshake_role: None,
        generation: 1,
        checkpointed_at: timestamp() + chrono::Duration::seconds(12),
    };

    let result = persist_private_stream_batch_with_link_lease(
        &storage,
        counterparty,
        vec![private_message(
            r#"{"version":1,"kind":"paykit.unknown","app_id":"bitkit"}"#,
        )],
        Some(link_state),
        None,
        Some(first_lease),
        timestamp() + chrono::Duration::seconds(12),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.private_stream_items.is_empty());
    assert!(snapshot.encrypted_link_states.is_empty());
    assert_eq!(snapshot.next_private_stream_item_id, 0);
}
