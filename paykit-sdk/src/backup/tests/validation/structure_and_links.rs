use super::*;
use crate::EncryptedLinkHandshakeRole;

#[tokio::test]
async fn test_restore_backup_state_rejects_duplicate_retired_apps() {
    let storage = InMemoryStorage::new();
    let app_id = paykit_lib::PaykitAppId::new("removed-app").unwrap();
    let backup = SdkBackupState {
        paykit_noise_public_key: None,
        allowance_accounting: None,
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(public_key())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: vec![app_id.clone(), app_id],
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
async fn test_restore_backup_state_rejects_deliverable_retired_app_message() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let app_id = app_id();
    let backup = SdkBackupState {
        paykit_noise_public_key: None,
        allowance_accounting: None,
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: vec![app_id],
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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
        paykit_noise_public_key: None,
        allowance_accounting: None,
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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

#[tokio::test]
async fn test_restore_backup_state_requires_matching_checkpoint_authorization() {
    let signer = pubky::Keypair::random();
    let counterparty = PubkyPublicKey::from_public_key(&signer.public_key());
    let matching = paykit_lib::PaykitNoiseKeyAuthorization::sign(&signer, &[7; 32], 1).unwrap();
    let mismatched = paykit_lib::PaykitNoiseKeyAuthorization::sign(&signer, &[8; 32], 2).unwrap();

    for state in [LinkedPeerState::Linked, LinkedPeerState::Linking] {
        for (authorization, has_peer) in [
            (Some(matching.clone()), true),
            (None, true),
            (Some(mismatched.clone()), true),
            (None, false),
        ] {
            let checkpoint = link_checkpoint(&counterparty, matching.noise_public_key(), &state);
            let mut backup = empty_backup(identity(public_key()));
            backup.encrypted_link_states.push(checkpoint.clone());
            if has_peer {
                backup.linked_peers.push(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: state.clone(),
                    last_sync_at: Some(timestamp()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                    noise_key_authorization: authorization.clone(),
                });
            }
            let can_resume = authorization.as_ref() == Some(&matching);
            if !can_resume {
                backup
                    .outbound_private_messages
                    .push(private_payment_list_outbound(
                        counterparty.clone(),
                        1,
                        "lnbc1example",
                    ));
            }
            let backup = serde_json::from_slice(&serde_json::to_vec(&backup).unwrap()).unwrap();
            let storage = InMemoryStorage::new();

            let report = restore_backup_state(&storage, backup).await.unwrap();

            let restored = storage.snapshot().unwrap();
            let peer = &restored.linked_peers[&counterparty];
            let link = &restored.encrypted_link_states[&counterparty];
            assert_eq!(peer.noise_key_authorization, authorization);
            if can_resume {
                assert!(report.recovery_required_peers.is_empty());
                assert_eq!(peer.state, state);
                assert_eq!(link, &checkpoint);
            } else {
                assert_eq!(report.recovery_required_peers, vec![counterparty.clone()]);
                assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
                assert!(link.link_snapshot.is_none());
                assert!(link.handshake_snapshot.is_none());
                assert!(link.handshake_role.is_none());
                assert_eq!(link.generation, checkpoint.generation + 1);
                assert_eq!(
                    restored.outbound_private_messages[0].status,
                    OutboundPrivateMessageStatus::RecoveryRequired
                );
            }
        }
    }
}

fn link_checkpoint(
    counterparty: &PubkyPublicKey,
    noise_public_key: &pubky::PublicKey,
    state: &LinkedPeerState,
) -> EncryptedLinkStateRecord {
    use pubky_noise::snow_crypto::{HandshakePattern, NoisePhase, NoiseStep};

    let linked = *state == LinkedPeerState::Linked;
    let snapshot = pubky_noise::serializer::PubkyNoiseSessionState {
        version: pubky_noise::serializer::SESSION_STATE_VERSION,
        phase: if linked {
            NoisePhase::Transport
        } else {
            NoisePhase::HandShake
        },
        pattern: HandshakePattern::PatternXX,
        initiator: true,
        ephemeral_secret: [1; 32],
        static_secret: Some([2; 32]),
        counter: if linked { 3 } else { 0 },
        noise_step: if linked {
            NoiseStep::Final
        } else {
            NoiseStep::StepOne
        },
        sub_step_index: 0,
        handshake_hash: linked.then_some([3; 32]),
        link_id: linked.then_some([4; 32]),
        sending_nonce: 0,
        receiving_nonce: 0,
        write_counter: if linked { 3 } else { 0 },
        read_counter: if linked { 3 } else { 0 },
        endpoint_pubkey: counterparty.to_public_key().unwrap().to_bytes(),
        handshake_messages: if linked {
            vec![vec![5; 96]]
        } else {
            Vec::new()
        },
    };
    let mut bytes = snapshot.serialize();
    bytes.extend_from_slice(&noise_public_key.to_bytes());
    // The Paykit snapshot appends the routing key and empty recovery context.
    bytes.extend_from_slice(&[0; 72]);
    if !linked {
        let mut envelope = vec![1];
        envelope.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        envelope.extend_from_slice(&bytes);
        envelope.extend_from_slice(&0_u16.to_be_bytes());
        bytes = envelope;
    }
    EncryptedLinkStateRecord {
        counterparty: counterparty.clone(),
        link_snapshot: linked.then(|| bytes.clone()),
        handshake_snapshot: (!linked).then_some(bytes),
        handshake_role: (!linked).then_some(EncryptedLinkHandshakeRole::Initiator),
        generation: 7,
        checkpointed_at: timestamp(),
    }
}

#[test]
fn test_restore_reconciliation_preserves_recovery_required_and_blocked_peers() {
    let signer = pubky::Keypair::random();
    let counterparty = PubkyPublicKey::from_public_key(&signer.public_key());
    let authorization =
        paykit_lib::PaykitNoiseKeyAuthorization::sign(&signer, &[7; 32], 1).unwrap();
    for state in [LinkedPeerState::RecoveryRequired, LinkedPeerState::Blocked] {
        let mut peers = std::collections::HashMap::from([(
            counterparty.clone(),
            LinkedPeerRecord {
                counterparty: counterparty.clone(),
                state: state.clone(),
                last_sync_at: Some(timestamp()),
                last_private_receive_at: None,
                failure_count: 1,
                local_recovery_attempt_id: None,
                local_recovery_marker_created_at: None,
                local_recovery_marker_last_error: None,
                remote_recovery_attempt_id: None,
                remote_recovery_marker_observed_at: None,
                noise_key_authorization: Some(authorization.clone()),
            },
        )]);
        let link_states = std::collections::HashMap::from([(
            counterparty.clone(),
            link_checkpoint(
                &counterparty,
                authorization.noise_public_key(),
                &LinkedPeerState::Linked,
            ),
        )]);

        let recovery_required =
            reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new()).unwrap();

        let expected = if state == LinkedPeerState::RecoveryRequired {
            vec![counterparty.clone()]
        } else {
            Vec::new()
        };
        assert_eq!(recovery_required, expected);
        assert_eq!(peers.get(&counterparty.clone()).unwrap().state, state);
    }
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
            noise_key_authorization: None,
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

    let recovery_required =
        reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new()).unwrap();

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
            noise_key_authorization: None,
        },
    )]);
    let link_states = std::collections::HashMap::new();

    let recovery_required =
        reconcile_restored_linked_peers(&mut peers, &link_states, &Vec::new()).unwrap();

    assert_eq!(recovery_required, vec![counterparty.clone()]);
    assert_eq!(
        peers.get(&counterparty.clone()).unwrap().state,
        LinkedPeerState::RecoveryRequired
    );
}

#[tokio::test]
async fn test_restore_backup_state_rejects_noise_key_authorization_for_another_counterparty() {
    let storage = InMemoryStorage::new();
    let local_identity = identity(public_key());
    storage
        .save_identity_state(local_identity.clone())
        .await
        .unwrap();
    let before = storage.snapshot().unwrap();
    let counterparty = public_key();
    let signer = pubky::Keypair::random();
    let authorization =
        paykit_lib::PaykitNoiseKeyAuthorization::sign(&signer, &[7; 32], 1).unwrap();
    assert_ne!(
        authorization.owner(),
        &counterparty.to_public_key().unwrap()
    );
    let mut backup = empty_backup(local_identity);
    backup.linked_peers.push(LinkedPeerRecord {
        counterparty,
        state: LinkedPeerState::RecoveryRequired,
        last_sync_at: Some(timestamp()),
        last_private_receive_at: None,
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        local_recovery_marker_last_error: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
        noise_key_authorization: Some(authorization),
    });
    // Deserialization verifies the signature before restore checks its owner.
    let backup = serde_json::from_slice(&serde_json::to_vec(&backup).unwrap()).unwrap();

    let error = restore_backup_state(&storage, backup).await.unwrap_err();

    assert!(matches!(error, PaykitSdkError::Protocol { context, .. }
        if context.contains("Noise key authorization belongs to another counterparty")));
    assert_eq!(storage.snapshot().unwrap(), before);
}

#[tokio::test]
async fn test_restore_backup_state_rejects_local_recovery_marker_without_created_at() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        paykit_noise_public_key: None,
        allowance_accounting: None,
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
            noise_key_authorization: None,
        }],
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_remote_recovery_attempt_id() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        paykit_noise_public_key: None,
        allowance_accounting: None,
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
            noise_key_authorization: None,
        }],
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_records_without_identity() {
    let storage = InMemoryStorage::new();
    let backup = SdkBackupState {
        paykit_noise_public_key: None,
        allowance_accounting: None,
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
