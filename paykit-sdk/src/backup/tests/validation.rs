use super::*;
use crate::EncryptedLinkHandshakeRole;

#[tokio::test]
async fn test_restore_backup_state_rejects_malformed_link_snapshot() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: vec![EncryptedLinkStateRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            link_snapshot: Some(vec![1, 2, 3]),
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

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_recovery_required_restore_state_drops_link_snapshots() {
    let counterparty = public_key();
    let mut states = std::collections::HashMap::from([(
        peer_key(&counterparty),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            link_snapshot: Some(vec![1]),
            handshake_snapshot: Some(vec![2]),
            handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
            generation: 7,
            checkpointed_at: timestamp(),
        },
    )]);

    clear_recovery_required_link_snapshots(
        &mut states,
        std::slice::from_ref(&peer_key(&counterparty)),
    );

    let state = states.get(&peer_key(&counterparty)).unwrap();
    assert!(state.link_snapshot.is_none());
    assert!(state.handshake_snapshot.is_none());
    assert!(state.handshake_role.is_none());
    assert_eq!(state.generation, 8);
}

#[test]
fn test_restore_reconciliation_preserves_active_link_checkpoint() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::from([(
        peer_key(&counterparty),
        LinkedPeerRecord {
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
        },
    )]);
    let link_states = std::collections::HashMap::from([(
        peer_key(&counterparty),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            link_snapshot: Some(vec![1]),
            handshake_snapshot: None,
            handshake_role: None,
            generation: 7,
            checkpointed_at: timestamp(),
        },
    )]);

    let recovery_required = reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new());

    assert!(recovery_required.is_empty());
    assert_eq!(
        peers.get(&peer_key(&counterparty)).unwrap().state,
        LinkedPeerState::Linked
    );
}

#[test]
fn test_restore_reconciliation_preserves_handshake_checkpoint() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::new();
    let link_states = std::collections::HashMap::from([(
        peer_key(&counterparty),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            link_snapshot: None,
            handshake_snapshot: Some(vec![2]),
            handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
            generation: 7,
            checkpointed_at: timestamp(),
        },
    )]);

    let recovery_required = reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new());

    assert!(recovery_required.is_empty());
    assert_eq!(
        peers.get(&peer_key(&counterparty)).unwrap().state,
        LinkedPeerState::Linking
    );
}

#[test]
fn test_restore_reconciliation_preserves_existing_recovery_required_peer() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::from([(
        peer_key(&counterparty),
        LinkedPeerRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            state: LinkedPeerState::RecoveryRequired,
            last_sync_at: Some(timestamp()),
            last_private_receive_at: None,
            failure_count: 1,
            local_recovery_attempt_id: None,
            local_recovery_marker_created_at: None,
            local_recovery_marker_last_error: None,
            remote_recovery_attempt_id: None,
            remote_recovery_marker_observed_at: None,
        },
    )]);
    let link_states = std::collections::HashMap::from([(
        peer_key(&counterparty),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            link_snapshot: Some(vec![1]),
            handshake_snapshot: None,
            handshake_role: None,
            generation: 7,
            checkpointed_at: timestamp(),
        },
    )]);

    let recovery_required = reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new());

    assert_eq!(recovery_required, vec![peer_key(&counterparty)]);
    assert_eq!(
        peers.get(&peer_key(&counterparty)).unwrap().state,
        LinkedPeerState::RecoveryRequired
    );
}

#[test]
fn test_restore_reconciliation_marks_missing_checkpoint_recovery_required() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::from([(
        peer_key(&counterparty),
        LinkedPeerRecord {
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
        },
    )]);
    let link_states = std::collections::HashMap::from([(
        peer_key(&counterparty),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: 7,
            checkpointed_at: timestamp(),
        },
    )]);

    let recovery_required = reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new());

    assert_eq!(recovery_required, vec![peer_key(&counterparty)]);
    assert_eq!(
        peers.get(&peer_key(&counterparty)).unwrap().state,
        LinkedPeerState::RecoveryRequired
    );
}

#[test]
fn test_restore_reconciliation_marks_missing_link_state_recovery_required() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::from([(
        peer_key(&counterparty),
        LinkedPeerRecord {
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
        },
    )]);
    let link_states = std::collections::HashMap::new();

    let recovery_required = reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new());

    assert_eq!(recovery_required, vec![peer_key(&counterparty)]);
    assert_eq!(
        peers.get(&peer_key(&counterparty)).unwrap().state,
        LinkedPeerState::RecoveryRequired
    );
}

#[tokio::test]
async fn test_restore_backup_state_rejects_local_recovery_marker_without_created_at() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: vec![LinkedPeerRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            state: LinkedPeerState::RecoveryRequired,
            last_sync_at: Some(timestamp()),
            last_private_receive_at: None,
            failure_count: 1,
            local_recovery_attempt_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
            local_recovery_marker_created_at: None,
            local_recovery_marker_last_error: None,
            remote_recovery_attempt_id: None,
            remote_recovery_marker_observed_at: None,
        }],
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_remote_recovery_attempt_id() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: vec![LinkedPeerRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            state: LinkedPeerState::RecoveryRequired,
            last_sync_at: Some(timestamp()),
            last_private_receive_at: None,
            failure_count: 1,
            local_recovery_attempt_id: None,
            local_recovery_marker_created_at: None,
            local_recovery_marker_last_error: None,
            remote_recovery_attempt_id: Some("not-a-uuid".into()),
            remote_recovery_marker_observed_at: Some(timestamp()),
        }],
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_records_without_identity() {
    let storage = InMemoryStorage::new();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: None,
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: vec![PublicEndpointRecord {
            identifier: "btc-lightning-bolt11".into(),
            payload: Some("ln".into()),
            status: crate::PublicationStatus::Published,
            updated_at: timestamp(),
            last_error: None,
        }],
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_public_endpoint_record() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: vec![PublicEndpointRecord {
            identifier: "private".into(),
            payload: Some("ln".into()),
            status: crate::PublicationStatus::Published,
            updated_at: timestamp(),
            last_error: None,
        }],
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_inconsistent_public_endpoint_status() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: vec![PublicEndpointRecord {
            identifier: "btc-lightning-bolt11".into(),
            payload: None,
            status: crate::PublicationStatus::Published,
            updated_at: timestamp(),
            last_error: None,
        }],
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_stale_private_stream_metadata() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json =
        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.to_owned();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty,
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    let item = &state.private_stream_items[0];
    assert_eq!(item.raw_json, raw_json);
    assert_eq!(
        item.parsed_kind.as_deref(),
        Some("paykit.private_payment_list")
    );
    assert_eq!(
        item.known_paykit_kind.as_deref(),
        Some("paykit.private_payment_list")
    );
    assert_eq!(item.parse_status, PrivateStreamParseStatus::Valid);
    assert_eq!(item.parse_error, None);
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_stale_private_stream_parse_status() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty,
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                    .into(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.private_payment_list".into()),
            known_paykit_kind: Some("paykit.private_payment_list".into()),
            parse_status: PrivateStreamParseStatus::MalformedRecognized,
            parse_error: Some("stale".into()),
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    let item = &state.private_stream_items[0];
    assert_eq!(item.parse_status, PrivateStreamParseStatus::Valid);
    assert_eq!(item.parse_error, None);
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_stale_private_stream_parse_error() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, _) = receipt_access_raw_with_context(
        "650e8400-e29b-41d4-a716-446655440000",
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let raw_json = raw_json.replace(
        &location,
        "/pub/paykit/v0/private/bitkit/wallet/receipts/not-the-receipt-id",
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty,
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::MalformedRecognized,
            parse_error: Some("stale".into()),
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    let item = &state.private_stream_items[0];
    assert_eq!(
        item.parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
    assert_ne!(item.parse_error.as_deref(), Some("stale"));
    assert!(item.parse_error.is_some());
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_stale_dedupe_event_header() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.payment_request".into()),
            known_paykit_kind: Some("paykit.payment_request".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            event_id: "750e8400-e29b-41d4-a716-446655440000".into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    assert_eq!(state.event_dedup_records.len(), 1);
    let record = state.event_dedup_records.values().next().unwrap();
    assert_eq!(record.event_id, "650e8400-e29b-41d4-a716-446655440000");
    assert_eq!(record.first_stream_item_id, 1);
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_overlapping_event_dedupe_membership() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.payment_request".into()),
            known_paykit_kind: Some("paykit.payment_request".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: vec![1],
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    let record = state.event_dedup_records.values().next().unwrap();
    assert_eq!(record.first_stream_item_id, 1);
    assert!(record.duplicate_stream_item_ids.is_empty());
    assert!(record.conflicting_stream_item_ids.is_empty());
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_wrong_receiver_receipt_access_dedupe_index() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, original_location, _) = receipt_access_raw_with_context(
        event_id,
        receipt_id.as_str(),
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let wrong_location = paykit_lib::ReceiptAccess::location(&other_receiver_path(), &receipt_id);
    let raw_json = raw_json.replace(&original_location, &wrong_location);
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::MalformedRecognized,
            parse_error: Some(
                "Receipt Access location does not match counterparty receiver bitkit".into(),
            ),
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    assert!(state.event_dedup_records.is_empty());
    assert!(state.receipt_access_records.is_empty());
    let item = &state.private_stream_items[0];
    assert_eq!(
        item.parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
    assert_eq!(
        item.parse_error.as_deref(),
        Some(crate::domain::private_stream::RECEIPT_ACCESS_RECEIVER_SCOPE_PARSE_ERROR)
    );
    // Normalization rewrote the old interpolated value; the stable summary
    // must not echo either receiver path.
    assert!(!item.parse_error.as_deref().unwrap().contains("bitkit"));
}

#[tokio::test]
async fn test_restore_of_pre_redaction_backup_succeeds() {
    // A backup exported before parse-error redaction stores serde-detail
    // summaries (offending values, serde positions, interpolated receiver
    // paths) that no current classifier ever produces. Restore must still
    // succeed: normalization rewrites the derived summaries from raw JSON
    // BEFORE stream-item validation byte-compares them against fresh
    // classifier output. The stale strings come verbatim from the frozen
    // legacy classification fixture, so this replays a real previous
    // generation, not a hand-authored approximation.
    use crate::domain::private_stream::classification_fixture::{
        classification_fixture_messages, classification_fixture_receiver_path,
        CLASSIFICATION_MATRIX_EXPECTED_JSON, CLASSIFICATION_MATRIX_EXPECTED_LEGACY_JSON,
    };

    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let messages = classification_fixture_messages();
    let legacy: serde_json::Value =
        serde_json::from_str(CLASSIFICATION_MATRIX_EXPECTED_LEGACY_JSON)
            .expect("legacy classification fixture must parse");
    let current: serde_json::Value = serde_json::from_str(CLASSIFICATION_MATRIX_EXPECTED_JSON)
        .expect("current classification fixture must parse");
    assert!(
        legacy["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["intake"]["parse_error"]
                    .as_str()
                    .is_some_and(|error| error.contains("at line 1 column"))
            }),
        "legacy summaries must include serde positional detail"
    );

    let private_stream_items: Vec<PrivateStreamItemRecord> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let intake = &legacy["messages"][index]["intake"];
            PrivateStreamItemRecord {
                stream_item_id: index as u64,
                counterparty: counterparty.clone(),
                counterparty_receiver_path: classification_fixture_receiver_path(),
                receive_batch_id: 0,
                raw_json: message.raw_json.clone(),
                parsed_version: intake["parsed_version"]
                    .as_u64()
                    .map(|version| version as u32),
                parsed_kind: intake["parsed_kind"].as_str().map(str::to_owned),
                known_paykit_kind: intake["known_paykit_kind"].as_str().map(str::to_owned),
                parse_status: serde_json::from_value(intake["parse_status"].clone()).unwrap(),
                parse_error: intake["parse_error"].as_str().map(str::to_owned),
                received_at: timestamp(),
            }
        })
        .collect();
    let event_dedup_records: Vec<EventDedupRecord> = legacy["event_dedup_records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| EventDedupRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: classification_fixture_receiver_path(),
            event_id: record["event_id"].as_str().unwrap().to_owned(),
            event_kind: record["event_kind"].as_str().unwrap().to_owned(),
            payload_hash: record["payload_hash"].as_str().unwrap().to_owned(),
            first_stream_item_id: record["first_stream_item_id"].as_u64().unwrap(),
            duplicate_stream_item_ids: serde_json::from_value(
                record["duplicate_stream_item_ids"].clone(),
            )
            .unwrap(),
            conflicting_stream_item_ids: serde_json::from_value(
                record["conflicting_stream_item_ids"].clone(),
            )
            .unwrap(),
        })
        .collect();
    // Local, non-rebuildable retrieval state recorded before the upgrade: the
    // indexed first carrier classifies identically under the current
    // classifier, so restore-time normalization must preserve it verbatim.
    let retrieved_at = timestamp();
    let receipt_access_records: Vec<ReceiptAccessRecord> = legacy["receipt_access_records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| ReceiptAccessRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: classification_fixture_receiver_path(),
            stream_item_id: record["stream_item_id"].as_u64().unwrap(),
            receive_batch_id: record["receive_batch_id"].as_u64().unwrap(),
            event_id: record["event_id"].as_str().unwrap().to_owned(),
            receipt_id: record["receipt_id"].as_str().unwrap().to_owned(),
            payment_reference: record["payment_reference"].as_str().unwrap().to_owned(),
            payment_request_id: record["payment_request_id"].as_str().map(str::to_owned),
            billing_period: serde_json::from_value(record["billing_period"].clone()).unwrap(),
            location: record["location"].as_str().unwrap().to_owned(),
            key: record["key"].as_str().unwrap().to_owned(),
            retrieval_status: crate::ReceiptRetrievalStatus::Retrieved,
            retrieval_attempted_at: Some(retrieved_at),
            retrieved_at: Some(retrieved_at),
            last_retrieval_error: None,
            received_at: timestamp(),
        })
        .collect();
    assert_eq!(receipt_access_records.len(), 1);
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items,
        event_dedup_records,
        receipt_access_records,
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: messages.len() as u64,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    assert_eq!(state.private_stream_items.len(), messages.len());
    for (index, item) in state.private_stream_items.iter().enumerate() {
        assert_eq!(item.raw_json, messages[index].raw_json, "item {index}");
        assert_eq!(
            item.parse_error.as_deref(),
            current["messages"][index]["intake"]["parse_error"].as_str(),
            "item {index} summary must be rewritten to the current generation"
        );
    }
    let access = state.receipt_access_records.values().next().unwrap();
    assert_eq!(
        access.retrieval_status,
        crate::ReceiptRetrievalStatus::Retrieved
    );
    assert_eq!(access.retrieved_at, Some(retrieved_at));
}

#[tokio::test]
async fn test_restore_backup_state_drops_receipt_record_orphaned_by_normalization() {
    // An older classifier indexed an out-of-scope Receipt Access event and a
    // Receipt was retrieved and cached under it. Normalization removes the
    // index, so the cached Receipt must be dropped with it instead of
    // failing the whole restore against a reference normalization removed.
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, original_location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id.as_str(),
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let wrong_location = paykit_lib::ReceiptAccess::location(&other_receiver_path(), &receipt_id);
    let raw_json = raw_json.replace(&original_location, &wrong_location);
    let access = ReceiptAccessRecord {
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
        stream_item_id: 1,
        receive_batch_id: 0,
        event_id: event_id.into(),
        receipt_id: receipt_id.as_str().into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: Some("750e8400-e29b-41d4-a716-446655440000".into()),
        billing_period: Some(period),
        location: wrong_location.clone(),
        key: key.clone(),
        retrieval_status: crate::ReceiptRetrievalStatus::Retrieved,
        retrieval_attempted_at: Some(timestamp()),
        retrieved_at: Some(timestamp()),
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = ReceiptRecord {
        issuer: counterparty.clone(),
        issuer_receiver_path: receiver_path(),
        receipt_access_event_id: access.event_id.clone(),
        receipt_access_key_hash: receipt_access_key_hash(&key),
        receipt_id: receipt_id.as_str().into(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: access.payment_request_id.clone(),
        billing_period: access.billing_period.clone(),
        recipient_public_key: counterparty.clone(),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location: access.location.clone(),
        retrieved_at: timestamp(),
    };
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: vec![access],
        receipt_records: vec![receipt],
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let report = restore_backup_state(&storage, backup).await.unwrap();

    assert_eq!(report.receipt_records, 0);
    let state = assert_normalization_fixpoint(&storage);
    assert!(state.event_dedup_records.is_empty());
    assert!(state.receipt_access_records.is_empty());
    assert!(state.receipt_records.is_empty());
    assert_eq!(
        state.private_stream_items[0].parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
}

#[tokio::test]
async fn test_restore_backup_state_accepts_cross_kind_event_id_conflict() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let request_json = payment_request_json(event_id);
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (receipt_access_json, _, _) = receipt_access_raw_with_context(
        event_id,
        "750e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "550e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![
            PrivateStreamItemRecord {
                stream_item_id: 1,
                counterparty: counterparty.clone(),
                counterparty_receiver_path: receiver_path(),
                receive_batch_id: 0,
                raw_json: request_json.clone(),
                parsed_version: Some(1),
                parsed_kind: Some("paykit.payment_request".into()),
                known_paykit_kind: Some("paykit.payment_request".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            },
            PrivateStreamItemRecord {
                stream_item_id: 2,
                counterparty: counterparty.clone(),
                counterparty_receiver_path: receiver_path(),
                receive_batch_id: 0,
                raw_json: receipt_access_json,
                parsed_version: Some(1),
                parsed_kind: Some("paykit.receipt_access".into()),
                known_paykit_kind: Some("paykit.receipt_access".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            },
        ],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            event_id: event_id.into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&request_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: vec![2],
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 3,
    };

    restore_backup_state(&storage, backup).await.unwrap();
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_missing_event_dedupe_index() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty,
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.payment_request".into()),
            known_paykit_kind: Some("paykit.payment_request".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    assert_eq!(state.event_dedup_records.len(), 1);
    let record = state.event_dedup_records.values().next().unwrap();
    assert_eq!(record.event_id, "650e8400-e29b-41d4-a716-446655440000");
    assert_eq!(record.first_stream_item_id, 1);
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_missing_receipt_access_index() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, _, _) = receipt_access_raw_with_context(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    assert_eq!(state.receipt_access_records.len(), 1);
    let record = state.receipt_access_records.values().next().unwrap();
    assert_eq!(record.stream_item_id, 1);
    assert_eq!(record.retrieval_status, ReceiptRetrievalStatus::Pending);
    assert_eq!(record.retrieval_attempted_at, None);
    assert_eq!(record.retrieved_at, None);
    assert_eq!(record.last_retrieval_error, None);
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_receipt_access_context_mismatch() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_reference = "invoice-2026-0001";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        payment_reference,
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![ReceiptAccessRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            stream_item_id: 1,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: receipt_id.into(),
            payment_reference: payment_reference.into(),
            payment_request_id: Some("850e8400-e29b-41d4-a716-446655440000".into()),
            billing_period: Some(period),
            location,
            key,
            retrieval_status: crate::ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at: timestamp(),
        }],
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    let record = state.receipt_access_records.values().next().unwrap();
    // The stale payment_request_id is replaced by re-derivation from the raw
    // payload, and the local retrieval state is reset with it.
    assert_eq!(
        record.payment_request_id.as_deref(),
        Some("750e8400-e29b-41d4-a716-446655440000")
    );
    assert_eq!(record.retrieval_status, ReceiptRetrievalStatus::Pending);
}

#[tokio::test]
async fn test_restore_backup_state_normalizes_receipt_access_receiver_mismatch() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_reference = "invoice-2026-0001";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, original_location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        payment_reference,
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let wrong_location = paykit_lib::ReceiptAccess::location(
        &other_receiver_path(),
        &paykit_lib::ReceiptId::new(receipt_id).unwrap(),
    );
    let raw_json = raw_json.replace(&original_location, &wrong_location);
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![ReceiptAccessRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            stream_item_id: 1,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: receipt_id.into(),
            payment_reference: payment_reference.into(),
            payment_request_id: Some("750e8400-e29b-41d4-a716-446655440000".into()),
            billing_period: Some(period),
            location: wrong_location,
            key,
            retrieval_status: crate::ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at: timestamp(),
        }],
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    // The out-of-scope Receipt Access event is no longer indexed at all; the
    // carrier item is downgraded by the receiver-scope policy instead.
    assert!(state.event_dedup_records.is_empty());
    assert!(state.receipt_access_records.is_empty());
    let item = &state.private_stream_items[0];
    assert_eq!(
        item.parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
}

#[tokio::test]
async fn test_restore_backup_state_rejects_inconsistent_receipt_access_status() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_reference = "invoice-2026-0001";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let payment_request_id = "750e8400-e29b-41d4-a716-446655440000";
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        payment_reference,
        payment_request_id,
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![ReceiptAccessRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            stream_item_id: 1,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: receipt_id.into(),
            payment_reference: payment_reference.into(),
            payment_request_id: Some(payment_request_id.into()),
            billing_period: Some(period),
            location,
            key,
            retrieval_status: crate::ReceiptRetrievalStatus::Retrieved,
            retrieval_attempted_at: None,
            retrieved_at: Some(timestamp()),
            last_retrieval_error: None,
            received_at: timestamp(),
        }],
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_normalizes_stale_derived_state_before_comparison() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let item = |stream_item_id: u64| PrivateStreamItemRecord {
        stream_item_id,
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
        receive_batch_id: 0,
        raw_json: raw_json.clone(),
        parsed_version: Some(1),
        parsed_kind: Some("paykit.receipt_access".into()),
        known_paykit_kind: Some("paykit.receipt_access".into()),
        parse_status: PrivateStreamParseStatus::Valid,
        parse_error: None,
        received_at: timestamp(),
    };
    // Stale derived state from an older classifier generation: a serde-detail
    // parse summary, flipped dedupe membership, and a Receipt Access index
    // that credits the duplicate carrier instead of the first one.
    let mut stale_first = item(1);
    stale_first.parse_status = PrivateStreamParseStatus::MalformedRecognized;
    stale_first.parse_error = Some("invalid type: string \"ten\", expected struct Amount".into());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![stale_first, item(2)],
        event_dedup_records: vec![EventDedupRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 2,
            duplicate_stream_item_ids: vec![1],
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: vec![ReceiptAccessRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            stream_item_id: 2,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            payment_reference: "invoice-2026-0001".into(),
            payment_request_id: Some("750e8400-e29b-41d4-a716-446655440000".into()),
            billing_period: Some(period),
            location,
            key,
            retrieval_status: crate::ReceiptRetrievalStatus::Retrieved,
            retrieval_attempted_at: Some(timestamp()),
            retrieved_at: Some(timestamp()),
            last_retrieval_error: None,
            received_at: timestamp(),
        }],
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 3,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let state = assert_normalization_fixpoint(&storage);
    let first = &state.private_stream_items[0];
    assert_eq!(first.parse_status, PrivateStreamParseStatus::Valid);
    assert_eq!(first.parse_error, None);
    let dedup = state.event_dedup_records.values().next().unwrap();
    assert_eq!(dedup.first_stream_item_id, 1);
    assert_eq!(dedup.duplicate_stream_item_ids, vec![2]);
    let access = state.receipt_access_records.values().next().unwrap();
    // The first carrier changed, so local retrieval state is reset.
    assert_eq!(access.stream_item_id, 1);
    assert_eq!(access.retrieval_status, ReceiptRetrievalStatus::Pending);
    assert_eq!(access.retrieval_attempted_at, None);
    assert_eq!(access.retrieved_at, None);
    assert_eq!(access.last_retrieval_error, None);
}

#[tokio::test]
async fn test_restore_rejects_tampered_immutable_context() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![ReceiptAccessRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            stream_item_id: 1,
            // Tampered immutable source context: the carrier item was received
            // in batch 0. All access-derived fields still match re-derivation,
            // so normalization keeps the record and validation rejects it.
            receive_batch_id: 7,
            event_id: event_id.into(),
            receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            payment_reference: "invoice-2026-0001".into(),
            payment_request_id: Some("750e8400-e29b-41d4-a716-446655440000".into()),
            billing_period: Some(period),
            location,
            key,
            retrieval_status: crate::ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at: timestamp(),
        }],
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_preserves_invalid_outbound_audit_record() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let mut invalid = private_payment_list_outbound(counterparty.clone(), 7, "ln-private");
    invalid.raw_json = "{malformed".into();
    invalid.status = OutboundPrivateMessageStatus::Invalid;
    invalid.last_error = Some("invalid private message JSON".into());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![invalid],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(restored.outbound_private_messages.len(), 1);
    assert_eq!(
        restored.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::Invalid
    );
    assert_eq!(restored.outbound_private_messages[0].raw_json, "{malformed");
}

#[tokio::test]
async fn test_restore_backup_state_preserves_recovery_required_outbound_audit_record() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let mut recovery_required =
        private_payment_list_outbound(counterparty.clone(), 7, "ln-private");
    recovery_required.raw_json = "{malformed".into();
    recovery_required.status = OutboundPrivateMessageStatus::RecoveryRequired;
    recovery_required.last_error = Some("Encrypted Link recovery is required".into());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![recovery_required],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(restored.outbound_private_messages.len(), 1);
    assert_eq!(
        restored.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    assert_eq!(restored.outbound_private_messages[0].raw_json, "{malformed");
}

#[tokio::test]
async fn test_restore_backup_state_marks_sending_outbound_recovery_required() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let mut sending = private_payment_list_outbound(counterparty.clone(), 7, "ln-private");
    sending.status = OutboundPrivateMessageStatus::Sending;
    sending.attempt_count = 1;
    sending.last_attempt_at = Some(timestamp());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![sending],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(
        restored.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    assert!(restored.outbound_private_messages[0]
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("recovery")));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_sent_outbound_without_sent_time() {
    let counterparty = public_key();
    let mut sent = private_payment_list_outbound(counterparty, 7, "ln-private");
    sent.status = OutboundPrivateMessageStatus::Sent;
    sent.attempt_count = 1;
    sent.last_attempt_at = Some(timestamp());

    assert_restore_rejects_outbound_record(sent).await;
}

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_outbound_without_error() {
    let counterparty = public_key();
    let mut invalid = private_payment_list_outbound(counterparty, 7, "ln-private");
    invalid.status = OutboundPrivateMessageStatus::Invalid;

    assert_restore_rejects_outbound_record(invalid).await;
}

#[tokio::test]
async fn test_restore_backup_state_rejects_recovery_required_outbound_with_sent_time() {
    let counterparty = public_key();
    let mut recovery_required = private_payment_list_outbound(counterparty, 7, "ln-private");
    recovery_required.status = OutboundPrivateMessageStatus::RecoveryRequired;
    recovery_required.last_error = Some("Encrypted Link recovery is required".into());
    recovery_required.sent_at = Some(timestamp());

    assert_restore_rejects_outbound_record(recovery_required).await;
}

#[tokio::test]
async fn test_restore_backup_state_rejects_wrong_identity() {
    let storage = InMemoryStorage::new();
    let current = public_key();
    let backup_public_key = public_key();
    storage
        .save_identity_state(identity(current))
        .await
        .unwrap();

    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(backup_public_key)),
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

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_wrong_receiver_noise_key() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    storage
        .save_identity_state(identity(local_public_key.clone()))
        .await
        .unwrap();
    let mut backup_identity = identity(local_public_key);
    backup_identity.local_receiver_noise_public_key = Some(public_key());
    let backup = SdkBackupState::from_storage_state(
        StorageState {
            identity_state: Some(backup_identity),
            ..StorageState::default()
        },
        receiver_path(),
    );

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_preserves_current_sign_out_generation() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let mut current_identity = identity(local_public_key.clone());
    current_identity.sign_out_generation = 7;
    storage.save_identity_state(current_identity).await.unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key.clone())),
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

    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .identity_state
            .unwrap()
            .sign_out_generation,
        7
    );
}

#[tokio::test]
async fn test_restore_backup_state_allows_trusted_identity_switch() {
    let storage = InMemoryStorage::new();
    let stored_public_key = public_key();
    let backup_public_key = public_key();
    storage
        .save_identity_state(identity(stored_public_key))
        .await
        .unwrap();
    let mut trusted_identity = identity(backup_public_key.clone());
    trusted_identity.sign_out_generation = 3;
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(backup_public_key.clone())),
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

    restore_backup_state_with_identity(&storage, backup, receiver_path(), Some(trusted_identity))
        .await
        .unwrap();

    let identity = storage.snapshot().unwrap().identity_state.unwrap();
    assert_eq!(
        identity.local_pubky_public_key.as_ref(),
        Some(&backup_public_key)
    );
    assert_eq!(identity.sign_out_generation, 3);
}

#[tokio::test]
async fn test_restore_identity_less_backup_preserves_signed_out_generation() {
    let storage = InMemoryStorage::new();
    storage
        .save_identity_state(signed_out_identity(7))
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

    let identity = storage.snapshot().unwrap().identity_state.unwrap();
    assert!(identity.local_pubky_public_key.is_none());
    assert_eq!(identity.sign_out_generation, 7);
}

#[tokio::test]
async fn test_restore_backup_state_rejects_orphan_endpoint_reservation() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
            reservation_id: "reservation-1".into(),
            counterparty,
            counterparty_receiver_path: receiver_path(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("ln-private"),
            outbound_message_id: 7,
            attribution: HashMap::new(),
            expires_at: None,
            cancellation_started_at: None,
            created_at: timestamp(),
        }],
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
async fn test_restore_backup_state_rejects_invalid_endpoint_reservation_id() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
            reservation_id: "reservation\n1".into(),
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("ln-private"),
            outbound_message_id: 7,
            attribution: HashMap::new(),
            expires_at: None,
            cancellation_started_at: None,
            created_at: timestamp(),
        }],
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![private_payment_list_outbound(
            counterparty,
            7,
            "ln-private",
        )],
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
async fn test_restore_backup_state_rejects_mismatched_endpoint_reservation_payload() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("different-payload"),
            outbound_message_id: 7,
            attribution: HashMap::new(),
            expires_at: None,
            cancellation_started_at: None,
            created_at: timestamp(),
        }],
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![private_payment_list_outbound(
            counterparty,
            7,
            "ln-private",
        )],
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
async fn test_restore_backup_state_rejects_receipt_key_hash_mismatch() {
    // The access record is fully backed by its stream item, so normalization
    // keeps it verbatim and the Receipt's key-hash mismatch still rejects.
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let access = ReceiptAccessRecord {
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
        stream_item_id: 1,
        receive_batch_id: 0,
        event_id: event_id.into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: Some("750e8400-e29b-41d4-a716-446655440000".into()),
        billing_period: Some(period),
        location: location.clone(),
        key,
        retrieval_status: crate::ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = ReceiptRecord {
        issuer: counterparty.clone(),
        issuer_receiver_path: receiver_path(),
        receipt_access_event_id: access.event_id.clone(),
        receipt_access_key_hash: receipt_access_key_hash("wrong-secret"),
        receipt_id: receipt_id.into(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: access.payment_request_id.clone(),
        billing_period: access.billing_period.clone(),
        recipient_public_key: counterparty.clone(),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location: access.location.clone(),
        retrieved_at: timestamp(),
    };
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            counterparty_receiver_path: receiver_path(),
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: vec![access],
        receipt_records: vec![receipt],
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_recipient_mismatch() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let issuer = public_key();
    let wrong_recipient = public_key();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let payment_request_id = "750e8400-e29b-41d4-a716-446655440000";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        "invoice-2026-0001",
        payment_request_id,
        &period,
    );
    let access = ReceiptAccessRecord {
        counterparty: issuer.clone(),
        counterparty_receiver_path: receiver_path(),
        stream_item_id: 1,
        receive_batch_id: 0,
        event_id: event_id.into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: Some(payment_request_id.into()),
        billing_period: Some(period.clone()),
        location: location.clone(),
        key: key.clone(),
        retrieval_status: crate::ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = ReceiptRecord {
        issuer: issuer.clone(),
        issuer_receiver_path: receiver_path(),
        receipt_access_event_id: event_id.into(),
        receipt_access_key_hash: receipt_access_key_hash(&key),
        receipt_id: receipt_id.into(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: access.payment_request_id.clone(),
        billing_period: access.billing_period.clone(),
        recipient_public_key: wrong_recipient,
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location,
        retrieved_at: timestamp(),
    };
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: issuer.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty: issuer,
            counterparty_receiver_path: receiver_path(),
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: vec![access],
        receipt_records: vec![receipt],
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_issuance_access_mismatch() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let counterparty = public_key();
    let prepared = paykit_lib::prepare_receipt_for_recipient(
        counterparty.to_public_key().unwrap(),
        &receiver_path(),
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
    let mut issuance =
        ReceiptIssuanceRecord::from_prepared(counterparty, receiver_path(), prepared, timestamp())
            .unwrap();
    issuance.payment_reference = "different-reference".into();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
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
        receipt_issuance_records: vec![issuance],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_redacts_invalid_receipt_issuance() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let counterparty = public_key();
    let prepared = paykit_lib::prepare_receipt_for_recipient(
        counterparty.to_public_key().unwrap(),
        &receiver_path(),
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
    let mut issuance =
        ReceiptIssuanceRecord::from_prepared(counterparty, receiver_path(), prepared, timestamp())
            .unwrap();
    let sentinel = "SENTINEL_PRIVATE_RECEIPT_CONTENT";
    issuance.encrypted_receipt = format!(r#"{{"sentinel":"{sentinel}""#);
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
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
        receipt_issuance_records: vec![issuance],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let err = restore_backup_state(&storage, backup).await.unwrap_err();

    assert!(matches!(
        &err,
        PaykitSdkError::Protocol { context, source }
            if context == "stored encrypted receipt is invalid" && source.is_none()
    ));
    let rendered = format!("{err} / {err:?}");
    assert!(
        !rendered.contains(sentinel),
        "stored encrypted Receipt leaked into Display/Debug: {rendered}"
    );
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_issuance_wrong_local_receiver() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let counterparty = public_key();
    let prepared = paykit_lib::prepare_receipt_for_recipient(
        counterparty.to_public_key().unwrap(),
        &other_receiver_path(),
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
    let issuance =
        ReceiptIssuanceRecord::from_prepared(counterparty, receiver_path(), prepared, timestamp())
            .unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
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
        receipt_issuance_records: vec![issuance],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_outbound_receipt_access_wrong_local_receiver() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let counterparty = public_key();
    let prepared = paykit_lib::prepare_receipt_for_recipient(
        counterparty.to_public_key().unwrap(),
        &other_receiver_path(),
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
    let issuance = ReceiptIssuanceRecord::from_prepared(
        counterparty.clone(),
        receiver_path(),
        prepared,
        timestamp(),
    )
    .unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![OutboundPrivateMessageRecord {
            outbound_message_id: 7,
            counterparty,
            counterparty_receiver_path: receiver_path(),
            kind: PrivateMessageKind::ReceiptAccess.as_str().into(),
            raw_json: issuance.access_json,
            status: OutboundPrivateMessageStatus::Pending,
            attempt_count: 0,
            created_at: timestamp(),
            updated_at: timestamp(),
            last_attempt_at: None,
            sent_at: None,
            last_error: None,
        }],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_duplicate_receipt_issuance_ids() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let first_counterparty = public_key();
    let second_counterparty = public_key();
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let draft = || paykit_lib::ReceiptDraft {
        receipt_id: Some(receipt_id.clone()),
        payment_reference: paykit_lib::PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        payment_endpoint_identifier: Some(
            paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        ),
        amount: Some(paykit_lib::PaymentAmount::new("0.001", "btc").unwrap()),
        metadata: serde_json::Map::new(),
    };
    let first = ReceiptIssuanceRecord::from_prepared(
        first_counterparty.clone(),
        receiver_path(),
        paykit_lib::prepare_receipt_for_recipient(
            first_counterparty.to_public_key().unwrap(),
            &receiver_path(),
            draft(),
        )
        .unwrap(),
        timestamp(),
    )
    .unwrap();
    let second = ReceiptIssuanceRecord::from_prepared(
        second_counterparty.clone(),
        receiver_path(),
        paykit_lib::prepare_receipt_for_recipient(
            second_counterparty.to_public_key().unwrap(),
            &receiver_path(),
            draft(),
        )
        .unwrap(),
        timestamp(),
    )
    .unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
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
        receipt_issuance_records: vec![first, second],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}
