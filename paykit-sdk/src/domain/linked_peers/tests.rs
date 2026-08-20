use chrono::{TimeZone, Utc};

use super::*;
use crate::{
    domain::outbound_private::OutboundPrivateMessageStatus,
    storage::{InMemoryStorage, NewOutboundPrivateMessage},
};

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 4, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

#[tokio::test]
async fn test_save_link_handshake_state_marks_peer_linking() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let report = save_link_handshake_state(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();

    assert_eq!(report.state, LinkedPeerState::Linking);
    assert_eq!(
        report.handshake_role,
        Some(EncryptedLinkHandshakeRole::Initiator)
    );
    let peer = load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linking);
    let link_state = load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.handshake_snapshot, Some(vec![1, 2, 3]));
    assert!(link_state.link_snapshot.is_none());
}

#[tokio::test]
async fn test_save_linked_peer_link_state_clears_pending_handshake() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_link_handshake_state(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Responder,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();

    let report =
        save_linked_peer_link_state(&storage, counterparty.clone(), vec![4, 5, 6], timestamp())
            .await
            .unwrap();

    assert_eq!(report.state, LinkedPeerState::Linked);
    assert_eq!(report.handshake_role, None);
    assert_eq!(report.generation, 1);
    let link_state = load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![4, 5, 6]));
    assert!(link_state.handshake_snapshot.is_none());
    assert!(link_state.handshake_role.is_none());
}

#[tokio::test]
async fn test_save_linked_peer_link_state_requeues_recovery_required_messages() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let active_app_id = paykit_lib::PaykitAppId::new("bitkit").unwrap();
    let retired_app_id = paykit_lib::PaykitAppId::new("retired-app").unwrap();
    let unregistered_app_id = paykit_lib::PaykitAppId::new("unregistered-app").unwrap();
    let active_payload = r#"{"version":1,"kind":"paykit.payment_request","app_id":"bitkit","event_id":"active-event"}"#.to_owned();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let active_app_id = active_app_id.clone();
            let retired_app_id = retired_app_id.clone();
            let unregistered_app_id = unregistered_app_id.clone();
            let active_payload = active_payload.clone();
            move |tx| {
                tx.activate_paykit_app(&active_app_id);
                tx.activate_paykit_app(&retired_app_id);
                tx.retire_paykit_app(retired_app_id.clone());
                for (app_id, payload) in [
                    (active_app_id, active_payload),
                    (
                        retired_app_id,
                        r#"{"version":1,"kind":"paykit.payment_request","app_id":"retired-app","event_id":"retired-event"}"#.to_owned(),
                    ),
                    (
                        unregistered_app_id,
                        r#"{"version":1,"kind":"paykit.payment_request","app_id":"unregistered-app","event_id":"unregistered-event"}"#.to_owned(),
                    ),
                ] {
                    let mut message = tx.insert_outbound_private_message(
                        NewOutboundPrivateMessage::new(
                            counterparty.clone(),
                            app_id,
                            "paykit.payment_request".into(),
                            payload,
                            timestamp(),
                        ),
                    );
                    message.status = OutboundPrivateMessageStatus::RecoveryRequired;
                    message.attempt_count = 1;
                    message.last_attempt_at = Some(timestamp());
                    message.last_error = Some("stale link".into());
                    tx.save_outbound_private_message(message)?;
                }
                Ok(())
            }
        })
        .await
        .unwrap();

    save_linked_peer_link_state(
        &storage,
        counterparty.clone(),
        vec![1, 2, 3],
        timestamp() + chrono::Duration::seconds(1),
    )
    .await
    .unwrap();

    let messages = storage
        .transaction(|tx| Ok(tx.outbound_private_messages(&counterparty)))
        .await
        .unwrap();
    let active = messages
        .iter()
        .find(|message| message.app_id == active_app_id)
        .unwrap();
    assert_eq!(active.raw_json, active_payload);
    assert_eq!(active.status, OutboundPrivateMessageStatus::Pending);
    assert_eq!(active.attempt_count, 0);
    assert!(active.last_attempt_at.is_none());
    assert!(active.last_error.is_none());
    for app_id in [&retired_app_id, &unregistered_app_id] {
        let message = messages
            .iter()
            .find(|message| &message.app_id == app_id)
            .unwrap();
        assert_eq!(
            message.status,
            OutboundPrivateMessageStatus::RecoveryRequired
        );
        assert_eq!(message.attempt_count, 1);
        assert!(message.last_attempt_at.is_some());
        assert_eq!(message.last_error.as_deref(), Some("stale link"));
    }

    storage
        .transaction({
            let counterparty = counterparty.clone();
            let unregistered_app_id = unregistered_app_id.clone();
            move |tx| {
                tx.activate_paykit_app(&unregistered_app_id);
                requeue_recovery_required_outbound_messages(tx, &counterparty, timestamp())
            }
        })
        .await
        .unwrap();
    let messages = storage
        .transaction(|tx| Ok(tx.outbound_private_messages(&counterparty)))
        .await
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .find(|message| message.app_id == unregistered_app_id)
            .unwrap()
            .status,
        OutboundPrivateMessageStatus::Pending
    );
    assert_eq!(
        messages
            .iter()
            .find(|message| message.app_id == retired_app_id)
            .unwrap()
            .status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
}

#[tokio::test]
async fn test_unleased_recovery_rejects_active_peer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_linked_peer_link_state(&storage, counterparty.clone(), vec![4, 5, 6], timestamp())
        .await
        .unwrap();
    storage
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

    let result =
        mark_recovery_required_inner(&storage, counterparty.clone(), None, timestamp()).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    let peer = load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    let link_state = load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![4, 5, 6]));
    assert!(link_state.handshake_snapshot.is_none());
    assert!(link_state.handshake_role.is_none());
    assert_eq!(link_state.generation, 0);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
        })
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_lease_checked_recovery_preserves_current_lease_for_caller() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_linked_peer_state(
        &storage,
        counterparty.clone(),
        LinkedPeerState::Linked,
        timestamp(),
    )
    .await
    .unwrap();
    let lease = storage
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

    mark_recovery_required_with_lease(
        &storage,
        counterparty.clone(),
        lease,
        timestamp() + chrono::Duration::seconds(1),
    )
    .await
    .unwrap();

    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
        })
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_unleased_state_save_allows_expired_peer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    storage
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

    let record = save_linked_peer_state(
        &storage,
        counterparty.clone(),
        LinkedPeerState::Linked,
        timestamp() + chrono::Duration::seconds(11),
    )
    .await
    .unwrap();

    assert_eq!(record.state, LinkedPeerState::Linked);
}

#[tokio::test]
async fn test_mark_recovery_required_clears_handshake_snapshot() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_link_handshake_state(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Responder,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();

    let mark = mark_recovery_required_inner(&storage, counterparty.clone(), None, timestamp())
        .await
        .unwrap();

    assert!(mark.new_episode);
    let peer = load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    let link_state = load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert!(link_state.link_snapshot.is_none());
    assert!(link_state.handshake_snapshot.is_none());
    assert!(link_state.handshake_role.is_none());
    assert_eq!(link_state.generation, 1);
}

#[tokio::test]
async fn test_save_link_handshake_state_rejects_blocked_peer() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_linked_peer_state(
        &storage,
        counterparty.clone(),
        LinkedPeerState::Blocked,
        timestamp(),
    )
    .await
    .unwrap();

    let result = save_link_handshake_state(
        &storage,
        counterparty,
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_generation_checked_handshake_save_keeps_newer_link() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_link_handshake_state(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();
    save_linked_peer_link_state(&storage, counterparty.clone(), vec![4, 5, 6], timestamp())
        .await
        .unwrap();

    let report = save_link_handshake_state_if_generation(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![7, 8, 9],
        0,
        timestamp(),
    )
    .await
    .unwrap();

    assert_eq!(report.state, LinkedPeerState::Linked);
    assert_eq!(report.generation, 1);
    let link_state = load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![4, 5, 6]));
    assert!(link_state.handshake_snapshot.is_none());
}

#[tokio::test]
async fn test_generation_checked_handshake_save_preserves_recovery_required() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_link_handshake_state(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();
    mark_recovery_required_inner(&storage, counterparty.clone(), None, timestamp())
        .await
        .unwrap();

    let report = save_link_handshake_state_if_generation(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![7, 8, 9],
        0,
        timestamp(),
    )
    .await
    .unwrap();

    assert_eq!(report.state, LinkedPeerState::RecoveryRequired);
    assert_eq!(report.generation, 1);
    assert_eq!(report.handshake_role, None);
    let peer = load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    let link_state = load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert!(link_state.link_snapshot.is_none());
    assert!(link_state.handshake_snapshot.is_none());
}

#[tokio::test]
async fn test_lease_checked_handshake_save_rejects_stale_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let first_lease = storage
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
    let active_lease = storage
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

    let result = save_link_handshake_state_with_lease(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        first_lease,
        timestamp() + chrono::Duration::seconds(12),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    let link_state = load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap();
    assert!(link_state.is_none());
    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
            })
            .await
            .unwrap(),
        Some(active_lease)
    );
}

#[tokio::test]
async fn test_lease_checked_recovery_rejects_stale_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_linked_peer_state(
        &storage,
        counterparty.clone(),
        LinkedPeerState::Linked,
        timestamp(),
    )
    .await
    .unwrap();

    let first_lease = storage
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
    let active_lease = storage
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

    let result = mark_recovery_required_with_lease(
        &storage,
        counterparty.clone(),
        first_lease,
        timestamp() + chrono::Duration::seconds(12),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    let peer = load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
            })
            .await
            .unwrap(),
        Some(active_lease)
    );
}
