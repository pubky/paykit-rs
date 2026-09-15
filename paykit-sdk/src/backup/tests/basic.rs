use super::*;

#[test]
fn test_public_only_backup_records_do_not_require_private_capability() {
    let identity_key = public_key();
    let contact_key = public_key();
    let mut backup = empty_backup(identity(identity_key));
    backup.contact_records.push(contact_record(contact_key));
    backup.public_endpoint_records.push(PublicEndpointRecord {
        app_id: app_id(),
        identifier: "btc-lightning-bolt11".into(),
        payload: Some("ln-public".into()),
        status: crate::PublicationStatus::Published,
        updated_at: timestamp(),
        last_error: None,
    });

    assert!(!backup.has_private_state());

    backup.linked_peers.push(LinkedPeerRecord {
        counterparty: public_key(),
        state: crate::LinkedPeerState::NotLinked,
        last_sync_at: None,
        last_private_receive_at: None,
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        local_recovery_marker_last_error: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
    });
    assert!(backup.has_private_state());
}

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
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 0,
                    checkpointed_at: timestamp(),
                });
                tx.insert_outbound_private_message(
                    crate::storage::NewOutboundPrivateMessage::new(
                        counterparty,
                        app_id(),
                        "paykit.private_payment_list".into(),
                        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{"btc-lightning-bolt11":"ln-private-payload-marker"}}"#.into(),
                        timestamp(),
                    ),
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();

    let backup = export_backup_state(&storage).await.unwrap();
    let debug = format!("{backup:?}");

    assert!(!debug.contains("ln-private-payload-marker"));
    assert!(!debug.contains("[1, 2, 3]"));
    assert!(!debug.contains(counterparty.as_str()));
    assert_eq!(backup.outbound_private_messages.len(), 1);
}

#[tokio::test]
async fn test_backup_round_trips_retired_apps() {
    let storage = InMemoryStorage::new();
    let retired_app = paykit_lib::PaykitAppId::new("removed-app").unwrap();
    storage
        .transaction({
            let retired_app = retired_app.clone();
            move |tx| {
                tx.save_identity_state(identity(public_key()));
                tx.retire_paykit_app(retired_app);
                Ok(())
            }
        })
        .await
        .unwrap();

    let backup = export_backup_state(&storage).await.unwrap();
    assert_eq!(backup.retired_paykit_apps, vec![retired_app.clone()]);

    let restored = InMemoryStorage::new();
    restore_backup_state(&restored, backup).await.unwrap();
    assert!(restored
        .snapshot()
        .unwrap()
        .retired_paykit_apps
        .contains(&retired_app));
}

#[tokio::test]
async fn test_backup_round_trips_payment_request_execution_claims() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let request_id = "550e8400-e29b-41d4-a716-446655440000";
    storage
        .save_identity_state(identity(public_key()))
        .await
        .unwrap();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000").replace(
        r#""required_app_id":null"#,
        r#""required_app_id":"paykit-server""#,
    );
    crate::domain::private_stream::persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![PrivateApplicationMessage {
            version: Some(1),
            kind: Some(PrivateMessageKind::PaymentRequest.as_str().into()),
            app_id: Some("bitkit".into()),
            raw_json,
        }],
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
                    payment_request_id: request_id.into(),
                    app_id: app_id(),
                    claimed_at: timestamp(),
                });
                Ok(())
            }
        })
        .await
        .unwrap();

    let backup = export_backup_state(&storage).await.unwrap();
    assert_eq!(backup.payment_request_execution_claims.len(), 1);

    let restored = InMemoryStorage::new();
    restore_backup_state(&restored, backup).await.unwrap();
    let claim = restored
        .snapshot()
        .unwrap()
        .payment_request_execution_claims
        .remove(&(counterparty, request_id.into()))
        .unwrap();

    assert_eq!(claim.app_id, app_id());
}

#[tokio::test]
async fn test_backup_restore_clears_receipt_app_authorization_cache() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(identity(public_key()));
                tx.save_authorized_paykit_apps(
                    counterparty,
                    HashMap::from([(
                        app_id(),
                        paykit_lib::PaykitAppCapabilities {
                            private_payments: false,
                            payment_requests: false,
                            receipts: true,
                            outgoing_payments: false,
                        },
                    )]),
                );
                Ok(())
            }
        })
        .await
        .unwrap();

    let backup = export_backup_state(&storage).await.unwrap();
    let restored = InMemoryStorage::new();
    restore_backup_state(&restored, backup).await.unwrap();

    assert!(restored
        .snapshot()
        .unwrap()
        .authorized_paykit_apps
        .is_empty());
}

#[tokio::test]
async fn test_restore_backup_state_marks_missing_link_checkpoint_recovery_required() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: vec![LinkedPeerRecord {
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
        }],
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
        encrypted_link_states: vec![EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
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

    assert_eq!(report.recovery_required_peers, vec![counterparty.clone()]);
    assert_eq!(
        restored
            .linked_peers
            .get(&counterparty.clone())
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

    let backup = export_backup_state(&storage).await.unwrap();
    let restore_storage = InMemoryStorage::new();
    let report = restore_backup_state(&restore_storage, backup)
        .await
        .unwrap();
    let restored = restore_storage.snapshot().unwrap();

    assert_eq!(report.contact_records, 1);
    assert_eq!(
        restored.contact_records[&contact_public_key]
            .label
            .as_deref(),
        Some("Alice")
    );
    assert!(report.recovery_required_peers.is_empty());
}

#[tokio::test]
async fn test_backup_round_trips_rejected_receipt_access_location() {
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
    let wrong_location = paykit_lib::ReceiptAccess::location_for(
        &ReceiptId::new("850e8400-e29b-41d4-a716-446655440000").unwrap(),
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
        app_id: value
            .get("app_id")
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
        vec![message],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let backup = export_backup_state(&storage).await.unwrap();
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
        Some("invalid data: Receipt Access location does not match Receipt ID")
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
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: vec![contact],
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
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
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: vec![contact],
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
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
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: vec![contact],
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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
async fn test_restore_backup_state_rejects_active_peer_work() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let lease = tx
                    .claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(60),
                    )?
                    .unwrap();
                assert_eq!(lease.lease_id, 0);
                Ok(())
            }
        })
        .await
        .unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: None,
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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

    let err = restore_backup_state(&storage, backup.clone())
        .await
        .unwrap_err();
    let snapshot = storage.snapshot().unwrap();

    assert!(matches!(err, PaykitSdkError::Policy { .. }));
    assert_eq!(snapshot.peer_link_operation_leases.len(), 1);
    assert_eq!(snapshot.next_peer_link_operation_lease_id, 1);

    restore_backup_state_with_identity(
        &storage,
        backup,
        None,
        timestamp() + chrono::Duration::seconds(61),
    )
    .await
    .unwrap();
    assert!(storage
        .snapshot()
        .unwrap()
        .peer_link_operation_leases
        .is_empty());
}

#[tokio::test]
async fn test_failed_restore_does_not_bind_empty_storage_to_identity() {
    let storage = InMemoryStorage::new();
    let trusted_identity = identity(public_key());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION + 1,
        identity_state: trusted_identity.clone().into(),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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

    let result =
        restore_backup_state_with_identity(&storage, backup, Some(trusted_identity), timestamp())
            .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
    assert!(storage.snapshot().unwrap().identity_state.is_none());
}

#[tokio::test]
async fn test_restore_backup_state_rejects_overwriting_existing_shared_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let existing_contact = public_key();
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let existing_contact = existing_contact.clone();
            move |tx| {
                tx.save_identity_state(identity(local_public_key));
                tx.save_contact_record(contact_record(existing_contact));
                Ok(())
            }
        })
        .await
        .unwrap();
    let before = storage.snapshot().unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(storage.snapshot().unwrap(), before);
}

#[tokio::test]
async fn test_restore_backup_state_accepts_matching_identity_only_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(identity(local_public_key));
                Ok(())
            }
        })
        .await
        .unwrap();
    let restored_contact = public_key();
    let mut backup = empty_backup(identity(local_public_key));
    backup
        .contact_records
        .push(contact_record(restored_contact.clone()));

    let report = restore_backup_state(&storage, backup).await.unwrap();

    assert!(report.restored_identity);
    assert_eq!(report.contact_records, 1);
    assert_eq!(
        storage.snapshot().unwrap().contact_records[&restored_contact].public_key,
        restored_contact
    );
}

#[tokio::test]
async fn test_restore_backup_state_rejects_registered_app_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(identity(local_public_key));
                tx.activate_paykit_app(&app_id());
                Ok(())
            }
        })
        .await
        .unwrap();
    let before = storage.snapshot().unwrap();

    let result = restore_backup_state(&storage, empty_backup(identity(local_public_key))).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(storage.snapshot().unwrap(), before);
}

#[tokio::test]
async fn test_restore_backup_state_rejects_advanced_counter_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(identity(local_public_key));
                tx.allocate_receive_batch_id()?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let before = storage.snapshot().unwrap();

    let result = restore_backup_state(&storage, empty_backup(identity(local_public_key))).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(storage.snapshot().unwrap(), before);
}
