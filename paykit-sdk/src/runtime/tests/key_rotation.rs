use super::*;
use crate::runtime::key_rotation::rotate_private_state;

fn key(byte: u8, generation: u64) -> crate::PaykitIdentitySecretKey {
    crate::PaykitIdentitySecretKey::new([byte; 32], generation).unwrap()
}

#[test]
fn test_replacement_key_requires_new_material_and_next_generation() {
    let current = key(7, 3);

    assert!(current.validate_successor(&key(8, 4)).is_ok());
    assert!(current.validate_successor(&key(8, 5)).is_err());
    assert!(current.validate_successor(&key(7, 4)).is_err());
}

#[tokio::test]
async fn test_key_rotation_preserves_history_and_resets_private_link_state() {
    let owner = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let storage = registered_test_storage();
    let raw_event = r#"{"version":1,"kind":"paykit.payment_request_cancellation","app_id":"bitkit","event_id":"650e8400-e29b-41d4-a716-446655440000","payment_request_id":"550e8400-e29b-41d4-a716-446655440000"}"#;
    storage
        .transaction({
            let owner = owner.clone();
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(owner),
                    initialized_at: FixedClock.now(),
                });
                tx.save_contact_record(crate::ContactRecord {
                    public_key: counterparty.clone(),
                    label: Some("Alice".into()),
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status:
                        crate::PublicationStatus::NotPublished,
                    public_contact_published_at: None,
                    public_contact_removed_at: None,
                    public_contact_last_error: None,
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: Some(FixedClock.now()),
                    failure_count: 2,
                    local_recovery_attempt_id: Some("old-local".into()),
                    local_recovery_marker_created_at: Some(FixedClock.now()),
                    local_recovery_marker_last_error: Some("old-error".into()),
                    remote_recovery_attempt_id: Some("old-remote".into()),
                    remote_recovery_marker_observed_at: Some(FixedClock.now()),
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                let mut message = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty.clone(),
                    app_id(),
                    PrivateMessageKind::PrivatePaymentList.as_str().into(),
                    r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#.into(),
                    FixedClock.now(),
                ))?;
                message.status = OutboundPrivateMessageStatus::Sending;
                message.prepared_send = Some(PreparedOutboundPrivateSend {
                    destination_path: "/pub/paykit/v0/private/send/0".into(),
                    ciphertext: vec![4, 5, 6],
                });
                tx.save_outbound_private_message(message)?;
                for confirmed in [false, true] {
                    let mut event = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        app_id(),
                        PrivateMessageKind::PaymentRequestCancellation.as_str().into(),
                        raw_event.into(),
                        FixedClock.now(),
                    ))?;
                    event.status = OutboundPrivateMessageStatus::Sent;
                    event.attempt_count = 1;
                    event.last_attempt_at = Some(FixedClock.now());
                    event.sent_at = Some(FixedClock.now());
                    event.confirmed_at = confirmed.then_some(FixedClock.now());
                    tx.save_outbound_private_message(event)?;
                }
                Ok(())
            }
        })
        .await
        .unwrap();

    storage
        .transaction({
            let owner = owner.clone();
            move |tx| rotate_private_state(tx, &owner, FixedClock.now())
        })
        .await
        .unwrap();

    let state = storage.snapshot().unwrap();
    assert_eq!(state.contact_records.len(), 1);
    assert!(state.encrypted_link_states.is_empty());
    assert!(state.peer_link_operation_leases.is_empty());
    let peer = state.linked_peers.get(&counterparty).unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    assert_eq!(peer.failure_count, 0);
    assert!(peer.local_recovery_attempt_id.is_none());
    assert!(peer.remote_recovery_attempt_id.is_none());
    assert_eq!(
        state.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    assert!(state.outbound_private_messages[0].prepared_send.is_none());
    let replay = &state.outbound_private_messages[1];
    assert_eq!(
        replay.status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    assert_eq!(replay.outbound_message_id, 1);
    assert_eq!(replay.raw_json, raw_event);
    assert_eq!(replay.app_id, app_id());
    assert_eq!(replay.attempt_count, 1);
    assert_eq!(replay.last_attempt_at, Some(FixedClock.now()));
    assert_eq!(replay.sent_at, Some(FixedClock.now()));
    assert!(replay.confirmed_at.is_none());
    assert!(replay.prepared_send.is_none());
    let confirmed = &state.outbound_private_messages[2];
    assert_eq!(confirmed.status, OutboundPrivateMessageStatus::Sent);
    assert_eq!(confirmed.confirmed_at, Some(FixedClock.now()));
}
