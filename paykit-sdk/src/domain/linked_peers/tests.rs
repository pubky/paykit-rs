use chrono::{TimeZone, Utc};

use super::*;
use crate::{
    domain::outbound_private::OutboundPrivateMessageStatus,
    storage::{
        BufferedPrivatePacketRecord, InMemoryStorage, NewOutboundPrivateMessage,
        PreparedOutboundPrivateSend, PrivateIntakeCursorRecord, StorageTransactionCallback,
        PRIVATE_INTAKE_PACKET_BYTES,
    },
};
use std::sync::{Arc, Mutex as StdMutex};

#[derive(Clone)]
struct TransactionObservedStorage {
    inner: InMemoryStorage,
    transaction_started: Arc<StdMutex<Option<std::sync::mpsc::Sender<()>>>>,
}

#[async_trait::async_trait]
impl StorageAdapter for TransactionObservedStorage {
    async fn load_local_identity_state(&self) -> Result<Option<crate::IdentityState>> {
        self.inner.load_local_identity_state().await
    }

    async fn transaction_erased<'a>(
        &self,
        callback: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        if let Some(started) = self.transaction_started.lock().unwrap().take() {
            started.send(()).unwrap();
        }
        self.inner.transaction_erased(callback).await
    }
}

#[derive(Clone)]
struct MutableClock(Arc<StdMutex<DateTime<Utc>>>);

impl MutableClock {
    fn new(now: DateTime<Utc>) -> Self {
        Self(Arc::new(StdMutex::new(now)))
    }

    fn set(&self, now: DateTime<Utc>) {
        *self.0.lock().unwrap() = now;
    }
}

impl crate::runtime::Clock for MutableClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 4, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

#[tokio::test]
async fn test_handshake_checkpoint_save_distinguishes_applied_from_stale() {
    use pubky_noise::{
        serializer::PubkyNoiseSessionState,
        snow_crypto::{HandshakePattern, NoisePhase, NoiseStep},
    };
    let counterparty = counterparty();
    let state = PubkyNoiseSessionState {
        version: pubky_noise::serializer::SESSION_STATE_VERSION,
        phase: NoisePhase::HandShake,
        pattern: HandshakePattern::PatternXX,
        initiator: true,
        ephemeral_secret: [1; 32],
        static_secret: Some([2; 32]),
        counter: 3,
        noise_step: NoiseStep::Final,
        sub_step_index: 0,
        handshake_hash: Some([3; 32]),
        link_id: None,
        sending_nonce: 0,
        receiving_nonce: 0,
        write_counter: 0,
        read_counter: 0,
        endpoint_pubkey: counterparty.to_public_key().unwrap().to_bytes(),
        handshake_messages: vec![vec![5; 96]],
    };
    let mut inner = state.serialize();
    inner.extend_from_slice(
        &pubky::Keypair::from_secret(&[5; 32])
            .public_key()
            .to_bytes(),
    );
    inner.extend_from_slice(&[0; 72]);
    let mut encoded = vec![1];
    encoded.extend_from_slice(&(inner.len() as u16).to_be_bytes());
    encoded.extend_from_slice(&inner);
    // Structural fixture: identity/path authorization is the caller's responsibility.
    let path = format!(
        "{}/{}/2",
        paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX,
        "a".repeat(64)
    );
    encoded.extend_from_slice(&(path.len() as u16).to_be_bytes());
    encoded.extend_from_slice(path.as_bytes());
    let mut packet = vec![0; pubky_noise::snow_crypto::PUBKY_NOISE_CIPHERTEXT_LEN + 2];
    packet[1] = 32;
    encoded.extend_from_slice(&packet);
    let snapshot = HandshakeCheckpointUpdate::Pending(
        paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(&encoded).unwrap(),
    );
    let storage = InMemoryStorage::new();
    save_link_handshake_state(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        timestamp(),
    )
    .await
    .unwrap();
    let (peer, source, lease) = storage
        .transaction(|tx| {
            let lease = tx
                .claim_peer_link_operation(
                    &counterparty,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(60),
                )?
                .unwrap();
            Ok((
                tx.linked_peer(&counterparty).unwrap(),
                tx.encrypted_link_state(&counterparty).unwrap(),
                lease,
            ))
        })
        .await
        .unwrap();
    let initial = storage.snapshot().unwrap();
    for mutation in [
        "none", "peer", "packet", "missing", "lease", "expiry", "linked", "recovery",
    ] {
        let mut before = initial.clone();
        let changed = before.encrypted_link_states.get_mut(&counterparty).unwrap();
        let changed_peer = before.linked_peers.get_mut(&counterparty).unwrap();
        match mutation {
            "peer" => {
                changed_peer.local_recovery_attempt_id =
                    Some("11111111-1111-4111-8111-111111111111".into())
            }
            "packet" => changed.handshake_snapshot.as_mut().unwrap().push(0),
            "linked" | "recovery" => {
                changed_peer.state = if mutation == "linked" {
                    LinkedPeerState::Linked
                } else {
                    LinkedPeerState::RecoveryRequired
                };
                changed.link_snapshot = (mutation == "linked").then(|| vec![4, 5, 6]);
                changed.handshake_snapshot = None;
                changed.handshake_role = None;
                changed.generation += 1;
            }
            "missing" => {
                before.encrypted_link_states.remove(&counterparty);
            }
            "lease" => {
                before
                    .peer_link_operation_leases
                    .get_mut(&counterparty)
                    .unwrap()
                    .claimed_at += chrono::Duration::seconds(1);
            }
            _ => {}
        }
        let storage = InMemoryStorage::from_state(before.clone());
        let now = if mutation == "expiry" {
            lease.expires_at
        } else {
            timestamp()
        };
        let result = storage
            .transaction(|tx| {
                save_handshake_checkpoint_in_transaction(tx, &peer, &source, &snapshot, &lease, now)
            })
            .await;
        if mutation == "none" {
            let HandshakeCheckpointSave::Applied(report) = result.unwrap() else {
                panic!("current checkpoint was not applied");
            };
            assert_eq!(report.state, LinkedPeerState::Linking);
            assert_eq!(report.generation, source.generation + 1);
            let after = storage.snapshot().unwrap();
            assert!(after.encrypted_link_states[&counterparty]
                .link_snapshot
                .is_none());
            assert_eq!(
                after.encrypted_link_states[&counterparty].handshake_snapshot,
                Some(encoded.clone())
            );
        } else {
            assert!(
                matches!(result, Ok(HandshakeCheckpointSave::Stale)),
                "{mutation}"
            );
            assert_eq!(storage.snapshot().unwrap(), before, "{mutation}");
        }
    }
}

#[test]
fn test_recovery_context_requires_both_tracked_attempt_ids() {
    let local = "11111111-1111-4111-8111-111111111111".to_owned();
    let remote = "22222222-2222-4222-8222-222222222222".to_owned();
    let mut peer = default_linked_peer(counterparty());
    let initial = paykit_lib::EncryptedLinkRecoveryContext::default();
    assert!(require_recovery_context(&peer, &initial).is_ok());
    peer.local_recovery_attempt_id = Some(local.clone());
    peer.remote_recovery_attempt_id = Some(remote.clone());
    let current =
        paykit_lib::EncryptedLinkRecoveryContext::new(Some(&local), Some(&remote)).unwrap();
    assert!(require_recovery_context(&peer, &current).is_ok());
    for context in [
        initial,
        paykit_lib::EncryptedLinkRecoveryContext::new(Some(&remote), Some(&remote)).unwrap(),
        paykit_lib::EncryptedLinkRecoveryContext::new(Some(&local), Some(&local)).unwrap(),
    ] {
        assert!(matches!(
            require_recovery_context(&peer, &context),
            Err(PaykitSdkError::RecoveryRequired { .. })
        ));
    }
}

#[tokio::test]
async fn test_recovery_replays_unconfirmed_events_in_original_order() {
    let app = paykit_lib::PaykitAppId::new("bitkit").unwrap();
    let storage = InMemoryStorage::with_registered_apps([app.clone()]);
    let peer = counterparty();
    let original = storage
        .transaction(|tx| {
            for index in 0..3 {
                let mut event =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        peer.clone(),
                        app.clone(),
                        "paykit.payment_request".into(),
                        format!("event-{index}"),
                        timestamp(),
                    ))?;
                event.attempt_count = 1;
                event.last_attempt_at = Some(timestamp());
                event = crate::domain::outbound_private::mark_outbound_sent(event, timestamp());
                if index == 1 {
                    event.confirmed_at = Some(timestamp());
                }
                tx.save_outbound_private_message(event)?;
            }
            Ok(tx.outbound_private_messages(&peer))
        })
        .await
        .unwrap();
    storage
        .transaction(|tx| {
            mark_recovery_required_in_transaction(tx, &peer, timestamp())?;
            Ok(())
        })
        .await
        .unwrap();
    save_linked_peer_link_state(&storage, peer.clone(), vec![1, 2, 3], timestamp())
        .await
        .unwrap();
    storage
        .transaction(|tx| {
            tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                peer.clone(),
                app,
                "paykit.payment_request".into(),
                "new-event".into(),
                timestamp(),
            ))?;
            let queued = tx.queued_outbound_private_messages(&peer);
            assert_eq!(
                queued
                    .iter()
                    .map(|message| message.outbound_message_id)
                    .collect::<Vec<_>>(),
                vec![0, 2, 3]
            );
            for id in [0, 2, 3] {
                let message = tx
                    .claim_next_outbound_private_message(
                        &peer,
                        timestamp(),
                        timestamp(),
                        timestamp() - chrono::Duration::seconds(1),
                    )
                    .unwrap();
                assert_eq!(message.outbound_message_id, id);
                assert!(message.prepared_send.is_none());
                if id < 3 {
                    assert_eq!(message.raw_json, original[id as usize].raw_json);
                    assert_eq!(message.sent_at, original[id as usize].sent_at);
                }
                tx.save_outbound_private_message(
                    crate::domain::outbound_private::mark_outbound_sent(message, timestamp()),
                )?;
            }
            assert!(tx
                .claim_next_outbound_private_message(
                    &peer,
                    timestamp(),
                    timestamp(),
                    timestamp() - chrono::Duration::seconds(1)
                )
                .is_none());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_recovery_requeues_retired_delivery_confirmation() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    let app = paykit_lib::PaykitAppId::new("bitkit").unwrap();
    storage
        .transaction(|tx| {
            tx.retire_paykit_app(app.clone());
            let mut message =
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    peer.clone(),
                    app,
                    "paykit.delivery_confirmation".into(),
                    "confirmation".into(),
                    timestamp(),
                ))?;
            message.status = OutboundPrivateMessageStatus::Failed;
            message.attempt_count = 1;
            message.last_attempt_at = Some(timestamp());
            message.last_error = Some("ambiguous publication".into());
            message.prepared_send = Some(crate::storage::PreparedOutboundPrivateSend {
                destination_path: "old-slot".into(),
                ciphertext: vec![1, 2, 3],
            });
            tx.save_outbound_private_message(message)?;
            mark_recovery_required_in_transaction(tx, &peer, timestamp())?;
            Ok(())
        })
        .await
        .unwrap();
    save_linked_peer_link_state(&storage, peer.clone(), vec![1, 2, 3], timestamp())
        .await
        .unwrap();
    let message =
        storage
            .transaction(|tx| {
                Ok(tx.claim_next_outbound_private_message(
                    &peer,
                    timestamp(),
                    timestamp(),
                    timestamp(),
                ))
            })
            .await
            .unwrap()
            .unwrap();
    assert!(message.is_delivery_confirmation());
    assert_eq!(message.raw_json, "confirmation");
    assert!(message.prepared_send.is_none());
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

    let unchanged = save_linked_peer_state(
        &storage,
        counterparty.clone(),
        LinkedPeerState::Linking,
        timestamp() + chrono::Duration::minutes(2),
    )
    .await
    .unwrap();
    assert_eq!(unchanged, peer);
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

    let peer = load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    let unchanged = save_linked_peer_state(
        &storage,
        counterparty,
        LinkedPeerState::Linked,
        timestamp() + chrono::Duration::minutes(2),
    )
    .await
    .unwrap();
    assert_eq!(unchanged, peer);
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
                    )?;
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
    assert_eq!(active.attempt_count, 1);
    assert_eq!(active.last_attempt_at, Some(timestamp()));
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

    mark_recovery_required_with_lease(
        &storage,
        counterparty.clone(),
        lease,
        &MutableClock::new(timestamp() + chrono::Duration::seconds(1)),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_lease_checked_recovery_expiry_after_storage_fence_is_atomic_and_reclaimable() {
    let inner = InMemoryStorage::new();
    let counterparty = counterparty();
    let claimed_at = timestamp();
    let expires_at = claimed_at + chrono::Duration::seconds(1);
    let authorization =
        paykit_lib::PaykitNoiseKeyAuthorization::sign(&pubky::Keypair::random(), &[1; 32], 1)
            .unwrap();
    let lease = inner
        .transaction({
            let counterparty = counterparty.clone();
            let authorization = authorization.clone();
            move |tx| {
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                peer.last_sync_at = Some(claimed_at);
                peer.noise_key_authorization = Some(authorization);
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: claimed_at,
                });
                let cursor = PrivateIntakeCursorRecord {
                    subscribed_user: counterparty.clone(),
                    homeserver: "https://homeserver.example".into(),
                    event_id: 9,
                };
                tx.advance_private_intake_cursor(cursor.clone())?;
                tx.buffer_private_intake_packet(BufferedPrivatePacketRecord {
                    counterparty: counterparty.clone(),
                    link_generation: 7,
                    receive_counter: 8,
                    ciphertext: vec![7; PRIVATE_INTAKE_PACKET_BYTES],
                    source_cursor: Some(cursor),
                    buffered_at: claimed_at,
                })?;
                let app_id = paykit_lib::PaykitAppId::new("bitkit").unwrap();
                tx.save_paykit_app_capabilities(
                    &app_id,
                    paykit_lib::PaykitAppCapabilities {
                        private_payments: true,
                        payment_requests: true,
                        receipts: true,
                        outgoing_payments: true,
                    },
                );
                tx.activate_paykit_app(&app_id);
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty.clone(),
                    app_id,
                    paykit_lib::PrivateMessageKind::PaymentRequest
                        .as_str()
                        .into(),
                    "{}".into(),
                    claimed_at,
                ))?;
                tx.claim_peer_link_operation(&counterparty, claimed_at, expires_at)?
                    .ok_or_else(|| PaykitSdkError::ConcurrentUpdate {
                        context: "test peer lease unavailable".into(),
                        source: None,
                    })
            }
        })
        .await
        .unwrap();
    let before = inner.snapshot().unwrap();
    let (holder_entered_tx, holder_entered_rx) = tokio::sync::oneshot::channel();
    let (release_holder_tx, release_holder_rx) = std::sync::mpsc::channel();
    let holder_storage = inner.clone();
    let holder = tokio::spawn(async move {
        holder_storage
            .transaction(move |_tx| {
                holder_entered_tx.send(()).unwrap();
                release_holder_rx.recv().unwrap();
                Ok(())
            })
            .await
    });
    holder_entered_rx.await.unwrap();

    let (recovery_started_tx, recovery_started_rx) = std::sync::mpsc::channel();
    let storage = TransactionObservedStorage {
        inner: inner.clone(),
        transaction_started: Arc::new(StdMutex::new(Some(recovery_started_tx))),
    };
    let recovery_counterparty = counterparty.clone();
    let recovery_lease = lease.clone();
    let clock = MutableClock::new(claimed_at);
    let recovery_clock = clock.clone();
    let recovery = tokio::spawn(async move {
        mark_recovery_required_with_lease(
            &storage,
            recovery_counterparty,
            recovery_lease,
            &recovery_clock,
        )
        .await
    });
    recovery_started_rx.recv().unwrap();
    clock.set(expires_at);
    release_holder_tx.send(()).unwrap();
    holder.await.unwrap().unwrap();

    assert!(matches!(
        recovery.await.unwrap(),
        Err(PaykitSdkError::Policy { .. })
    ));
    assert_eq!(inner.snapshot().unwrap(), before);

    let reclaimed_lease = inner
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    expires_at,
                    expires_at + chrono::Duration::minutes(1),
                )?
                .ok_or_else(|| PaykitSdkError::ConcurrentUpdate {
                    context: "test peer lease was not reclaimable at expiry boundary".into(),
                    source: None,
                })
            }
        })
        .await
        .unwrap();
    assert_ne!(reclaimed_lease.lease_id, lease.lease_id);
    mark_recovery_required_with_lease(&inner, counterparty.clone(), reclaimed_lease, &clock)
        .await
        .unwrap();
    let recovered = inner.snapshot().unwrap();
    assert_eq!(
        recovered.linked_peers[&counterparty].state,
        LinkedPeerState::RecoveryRequired
    );
    assert_eq!(
        recovered.linked_peers[&counterparty].last_sync_at,
        Some(expires_at)
    );
    assert_eq!(
        recovered.linked_peers[&counterparty]
            .noise_key_authorization
            .as_ref(),
        Some(&authorization)
    );
    assert!(recovered.encrypted_link_states[&counterparty]
        .link_snapshot
        .is_none());
    assert_eq!(
        recovered.encrypted_link_states[&counterparty].checkpointed_at,
        expires_at
    );
    assert_eq!(
        recovered.private_intake_cursors,
        before.private_intake_cursors
    );
    let intake = &recovered.private_intake_peers[&counterparty];
    assert_eq!(intake.link_generation, 8);
    assert!(intake.buffered_packets.is_empty());
    assert_eq!(
        recovered.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    assert_eq!(
        recovered.outbound_private_messages[0].updated_at,
        expires_at
    );
}

#[tokio::test]
async fn test_unleased_state_save_allows_expired_peer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    storage
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
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut message = tx.insert_outbound_private_message(
                    NewOutboundPrivateMessage::new(
                        counterparty,
                        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
                        "paykit.payment_request".into(),
                        r#"{"version":1,"kind":"paykit.payment_request","app_id":"bitkit","event_id":"event-1"}"#.into(),
                        timestamp(),
                    ),
                )?;
                message.status = OutboundPrivateMessageStatus::Sending;
                message.prepared_send = Some(PreparedOutboundPrivateSend {
                    destination_path: "/pub/paykit/v0/private/send/0".into(),
                    ciphertext: vec![4, 5, 6],
                });
                tx.save_outbound_private_message(message)
            }
        })
        .await
        .unwrap();

    mark_recovery_required_inner(&storage, counterparty.clone(), None, timestamp())
        .await
        .unwrap();

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
    let outbound = storage
        .transaction(move |tx| Ok(tx.outbound_private_messages(&counterparty)))
        .await
        .unwrap();
    assert_eq!(
        outbound[0].status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    assert!(outbound[0].prepared_send.is_none());
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
async fn test_lease_checked_handshake_save_rejects_stale_lease() {
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
    let active_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp() + chrono::Duration::seconds(11),
                    timestamp() + chrono::Duration::seconds(71),
                )
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
        None,
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
    let active_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp() + chrono::Duration::seconds(11),
                    timestamp() + chrono::Duration::seconds(71),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();

    let result = mark_recovery_required_with_lease(
        &storage,
        counterparty.clone(),
        first_lease,
        &MutableClock::new(timestamp() + chrono::Duration::seconds(12)),
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
