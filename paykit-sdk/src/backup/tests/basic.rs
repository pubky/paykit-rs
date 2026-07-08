use super::*;

#[tokio::test]
async fn test_export_backup_state_redacts_debug() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_identity_state(identity(counterparty.clone()));
                    tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                        counterparty: counterparty.clone(),
                counterparty_receiver_path: receiver_path(),
                        link_snapshot: Some(vec![1, 2, 3]),
                        handshake_snapshot: None,
                        handshake_role: None,
                        generation: 0,
                        checkpointed_at: timestamp(),
                    });
                    tx.insert_outbound_private_message(crate::storage::NewOutboundPrivateMessage::new(
                        counterparty,
                receiver_path(),
                        "paykit.private_payment_list".into(),
                        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"btc-lightning-bolt11":"ln-private-payload-marker"}}"#.into(),
                        timestamp(),
                    ));
                    Ok(())
                }
            })
            .await
            .unwrap();

    let backup = export_backup_state(&storage, receiver_path())
        .await
        .unwrap();
    let debug = format!("{backup:?}");

    assert_eq!(backup.local_receiver_path, receiver_path());
    assert!(!debug.contains("ln-private-payload-marker"));
    assert!(!debug.contains("[1, 2, 3]"));
    assert!(!debug.contains(counterparty.as_str()));
    assert_eq!(backup.outbound_private_messages.len(), 1);
}

#[tokio::test]
async fn test_restore_backup_state_rejects_wrong_local_receiver_path() {
    let storage = InMemoryStorage::new();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: other_receiver_path(),
        identity_state: None,
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_marks_missing_link_checkpoint_recovery_required() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: vec![LinkedPeerRecord {
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
        }],
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: vec![EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: 0,
            checkpointed_at: timestamp(),
        }],
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let report = restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(
        report.recovery_required_peers,
        vec![recovery_required_peer(&counterparty)]
    );
    assert_eq!(
        restored
            .linked_peers
            .get(&peer_key(&counterparty))
            .unwrap()
            .state,
        LinkedPeerState::RecoveryRequired
    );
    assert!(restored.peer_link_operation_leases.is_empty());
}

#[tokio::test]
async fn test_backup_state_round_trips_contact_records() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let contact_public_key = public_key();
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_identity_state(identity(local_public_key));
                tx.save_contact_record(contact_record(contact_public_key));
                Ok(())
            }
        })
        .await
        .unwrap();

    let backup = export_backup_state(&storage, receiver_path())
        .await
        .unwrap();
    let restore_storage = InMemoryStorage::new();
    let report = restore_backup_state(&restore_storage, backup)
        .await
        .unwrap();
    let restored = restore_storage.snapshot().unwrap();

    assert_eq!(report.contact_records, 1);
    assert_eq!(
        restored.contact_records[&peer_key(&contact_public_key)]
            .label
            .as_deref(),
        Some("Alice")
    );
    assert!(report.recovery_required_peers.is_empty());
}

#[tokio::test]
async fn test_backup_round_trips_receiver_rejected_receipt_access() {
    let storage = InMemoryStorage::new();
    let restored_storage = InMemoryStorage::new();
    let counterparty = public_key();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let billing_period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, _) = receipt_access_raw_with_context(
        "650e8400-e29b-41d4-a716-446655440000",
        receipt_id,
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &billing_period,
    );
    let wrong_location = paykit_lib::ReceiptAccess::location(
        &other_receiver_path(),
        &ReceiptId::new(receipt_id).unwrap(),
    );
    let raw_json = raw_json.replace(&location, &wrong_location);
    let value: serde_json::Value = serde_json::from_str(&raw_json).unwrap();
    let message = paykit_lib::PrivateApplicationMessage {
        version: value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u8::try_from(version).ok()),
        kind: value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        raw_json,
    };

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(identity(counterparty));
                Ok(())
            }
        })
        .await
        .unwrap();
    crate::domain::private_stream::persist_private_stream_batch(
        &storage,
        counterparty,
        receiver_path(),
        vec![message],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let backup = export_backup_state(&storage, receiver_path())
        .await
        .unwrap();
    let report = restore_backup_state(&restored_storage, backup)
        .await
        .unwrap();

    assert!(report.recovery_required_peers.is_empty());
    let restored = restored_storage.snapshot().unwrap();
    assert_eq!(
        restored.private_stream_items[0].parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
    assert_eq!(
        restored.private_stream_items[0].parse_error.as_deref(),
        Some("Receipt Access location does not match counterparty receiver bitkit/wallet")
    );
    assert!(restored.receipt_access_records.is_empty());
}

#[tokio::test]
async fn test_restore_backup_state_rejects_inconsistent_contact_marker_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let contact_public_key = public_key();
    let mut contact = contact_record(contact_public_key);
    contact.public_contact_published_at = Some(timestamp());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: vec![contact],
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_dual_contact_marker_timestamps() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let contact_public_key = public_key();
    let mut contact = contact_record(contact_public_key)
        .mark_public_contact_published(timestamp())
        .mark_public_contact_removed(timestamp());
    contact.public_contact_published_at = Some(timestamp());
    contact.public_contact_marker_status = crate::PublicationStatus::Failed;
    contact.public_contact_last_error = Some("failed".into());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: vec![contact],
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_accepts_pending_contact_marker_removal() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let contact_public_key = public_key();
    let contact = contact_record(contact_public_key)
        .mark_public_contact_published(timestamp())
        .mark_public_contact_removal_pending(timestamp());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: vec![contact],
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let report = restore_backup_state(&storage, backup).await.unwrap();

    assert_eq!(report.contact_records, 1);
}

#[tokio::test]
async fn test_restore_backup_state_preserves_next_peer_lease_id() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let lease = tx
                    .claim_peer_link_operation(
                        &counterparty,
                        &receiver_path(),
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(60),
                    )
                    .unwrap();
                assert_eq!(lease.lease_id, 0);
                Ok(())
            }
        })
        .await
        .unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: None,
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let snapshot = storage.snapshot().unwrap();

    assert!(snapshot.peer_link_operation_leases.is_empty());
    assert_eq!(snapshot.next_peer_link_operation_lease_id, 1);
}
