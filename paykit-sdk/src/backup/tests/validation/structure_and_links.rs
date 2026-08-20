use super::*;
use crate::EncryptedLinkHandshakeRole;

#[tokio::test]
async fn test_restore_backup_state_rejects_duplicate_retired_apps() {
    let storage = InMemoryStorage::new();
    let app_id = paykit_lib::PaykitAppId::new("removed-app").unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(public_key())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: vec![app_id.clone(), app_id],
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
async fn test_restore_backup_state_rejects_deliverable_retired_app_message() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let app_id = app_id();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: vec![app_id],
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![private_payment_list_outbound(
            counterparty,
            1,
            "lnbc1example",
        )],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 2,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_malformed_link_snapshot() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: vec![EncryptedLinkStateRecord {
            counterparty,
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
        counterparty.clone(),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: Some(vec![1]),
            handshake_snapshot: Some(vec![2]),
            handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
            generation: 7,
            checkpointed_at: timestamp(),
        },
    )]);

    clear_recovery_required_link_snapshots(
        &mut states,
        std::slice::from_ref(&counterparty.clone()),
    );

    let state = states.get(&counterparty.clone()).unwrap();
    assert!(state.link_snapshot.is_none());
    assert!(state.handshake_snapshot.is_none());
    assert!(state.handshake_role.is_none());
    assert_eq!(state.generation, 8);
}

#[test]
fn test_restore_reconciliation_preserves_active_link_checkpoint() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::from([(
        counterparty.clone(),
        LinkedPeerRecord {
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
        },
    )]);
    let link_states = std::collections::HashMap::from([(
        counterparty.clone(),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
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
        peers.get(&counterparty.clone()).unwrap().state,
        LinkedPeerState::Linked
    );
}

#[test]
fn test_restore_reconciliation_preserves_handshake_checkpoint() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::new();
    let link_states = std::collections::HashMap::from([(
        counterparty.clone(),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
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
        peers.get(&counterparty.clone()).unwrap().state,
        LinkedPeerState::Linking
    );
}

#[test]
fn test_restore_reconciliation_preserves_existing_recovery_required_peer() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::from([(
        counterparty.clone(),
        LinkedPeerRecord {
            counterparty: counterparty.clone(),
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
        counterparty.clone(),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: Some(vec![1]),
            handshake_snapshot: None,
            handshake_role: None,
            generation: 7,
            checkpointed_at: timestamp(),
        },
    )]);

    let recovery_required = reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new());

    assert_eq!(recovery_required, vec![counterparty.clone()]);
    assert_eq!(
        peers.get(&counterparty.clone()).unwrap().state,
        LinkedPeerState::RecoveryRequired
    );
}

#[test]
fn test_restore_reconciliation_marks_missing_checkpoint_recovery_required() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::from([(
        counterparty.clone(),
        LinkedPeerRecord {
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
        },
    )]);
    let link_states = std::collections::HashMap::from([(
        counterparty.clone(),
        EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: 7,
            checkpointed_at: timestamp(),
        },
    )]);

    let recovery_required = reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new());

    assert_eq!(recovery_required, vec![counterparty.clone()]);
    assert_eq!(
        peers.get(&counterparty.clone()).unwrap().state,
        LinkedPeerState::RecoveryRequired
    );
}

#[test]
fn test_restore_reconciliation_marks_missing_link_state_recovery_required() {
    let counterparty = public_key();
    let mut peers = std::collections::HashMap::from([(
        counterparty.clone(),
        LinkedPeerRecord {
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
        },
    )]);
    let link_states = std::collections::HashMap::new();

    let recovery_required = reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new());

    assert_eq!(recovery_required, vec![counterparty.clone()]);
    assert_eq!(
        peers.get(&counterparty.clone()).unwrap().state,
        LinkedPeerState::RecoveryRequired
    );
}

#[tokio::test]
async fn test_restore_backup_state_rejects_local_recovery_marker_without_created_at() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: vec![LinkedPeerRecord {
            counterparty,
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
        retired_paykit_apps: Vec::new(),
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
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: vec![LinkedPeerRecord {
            counterparty,
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
        retired_paykit_apps: Vec::new(),
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
        identity_state: None,
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: vec![PublicEndpointRecord {
            app_id: app_id(),
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
