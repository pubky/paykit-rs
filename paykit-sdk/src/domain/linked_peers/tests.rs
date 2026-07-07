use chrono::{TimeZone, Utc};

use super::*;
use crate::storage::InMemoryStorage;

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 4, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn receiver_id() -> PaykitReceiverId {
    PaykitReceiverId::new("bitkit").unwrap()
}

#[tokio::test]
async fn test_save_link_handshake_state_marks_peer_linking() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let report = save_link_handshake_state(
        &storage,
        counterparty.clone(),
        receiver_id(),
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
    let peer = load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linking);
    let link_state = load_encrypted_link_state(&storage, &counterparty, &receiver_id())
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
        receiver_id(),
        EncryptedLinkHandshakeRole::Responder,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();

    let report = save_linked_peer_link_state(
        &storage,
        counterparty.clone(),
        receiver_id(),
        vec![4, 5, 6],
        timestamp(),
    )
    .await
    .unwrap();

    assert_eq!(report.state, LinkedPeerState::Linked);
    assert_eq!(report.handshake_role, None);
    assert_eq!(report.generation, 1);
    let link_state = load_encrypted_link_state(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![4, 5, 6]));
    assert!(link_state.handshake_snapshot.is_none());
    assert!(link_state.handshake_role.is_none());
}

#[tokio::test]
async fn test_unleased_recovery_rejects_active_peer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_linked_peer_link_state(
        &storage,
        counterparty.clone(),
        receiver_id(),
        vec![4, 5, 6],
        timestamp(),
    )
    .await
    .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_id(),
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(10),
                ))
            }
        })
        .await
        .unwrap()
        .unwrap();

    let result = mark_recovery_required_inner(
        &storage,
        counterparty.clone(),
        Some(receiver_id()),
        None,
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    let peer = load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    let link_state = load_encrypted_link_state(&storage, &counterparty, &receiver_id())
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
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_id()))
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
        receiver_id(),
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
                    &receiver_id(),
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
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_id()))
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
                    &receiver_id(),
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
        receiver_id(),
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
        receiver_id(),
        EncryptedLinkHandshakeRole::Responder,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();

    let mark = mark_recovery_required_inner(
        &storage,
        counterparty.clone(),
        Some(receiver_id()),
        None,
        timestamp(),
    )
    .await
    .unwrap();

    assert!(mark.new_episode);
    let peer = load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    let link_state = load_encrypted_link_state(&storage, &counterparty, &receiver_id())
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
        receiver_id(),
        LinkedPeerState::Blocked,
        timestamp(),
    )
    .await
    .unwrap();

    let result = save_link_handshake_state(
        &storage,
        counterparty,
        receiver_id(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
}

#[tokio::test]
async fn test_generation_checked_handshake_save_keeps_newer_link() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    save_link_handshake_state(
        &storage,
        counterparty.clone(),
        receiver_id(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();
    save_linked_peer_link_state(
        &storage,
        counterparty.clone(),
        receiver_id(),
        vec![4, 5, 6],
        timestamp(),
    )
    .await
    .unwrap();

    let report = save_link_handshake_state_if_generation(
        &storage,
        counterparty.clone(),
        receiver_id(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![7, 8, 9],
        0,
        timestamp(),
    )
    .await
    .unwrap();

    assert_eq!(report.state, LinkedPeerState::Linked);
    assert_eq!(report.generation, 1);
    let link_state = load_encrypted_link_state(&storage, &counterparty, &receiver_id())
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
        receiver_id(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();
    mark_recovery_required_inner(
        &storage,
        counterparty.clone(),
        Some(receiver_id()),
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let report = save_link_handshake_state_if_generation(
        &storage,
        counterparty.clone(),
        receiver_id(),
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
    let peer = load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    let link_state = load_encrypted_link_state(&storage, &counterparty, &receiver_id())
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
                    &receiver_id(),
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
                    &receiver_id(),
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

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    let link_state = load_encrypted_link_state(&storage, &counterparty, &receiver_id())
        .await
        .unwrap();
    assert!(link_state.is_none());
    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_id()))
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
        receiver_id(),
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
                    &receiver_id(),
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
                    &receiver_id(),
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

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    let peer = load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_id()))
            })
            .await
            .unwrap(),
        Some(active_lease)
    );
}
