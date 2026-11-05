use super::harness::*;
use paykit_sdk::storage::{
    decode_storage_state_blob, encode_storage_state_blob, EncryptedLinkStateRecord,
    PreparedOutboundPrivateSend,
};
use paykit_sdk::{LinkedPeerState, OutboundPrivateMessageStatus, StorageAdapter};

#[tokio::test]
async fn test_withdrawal_completed_receipt_survives_rotation_only_for_original_grant() {
    let pair = linked_two_party().await;
    let original = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(pair.bob.public_key.clone())
        .await
        .unwrap();
    let ack = pair
        .alice
        .sdk
        .retry_private_payment_list_withdrawal(original.clone())
        .await
        .unwrap();
    assert_eq!(ack.withdrawal, original);
    let identity = pair.alice.access.local_secret_key.as_ref().unwrap();
    let replacement = identity.derive_paykit_identity_secret_key(2).unwrap();
    pair.alice
        .sdk
        .rotate_paykit_identity_key(replacement.clone())
        .await
        .unwrap();

    let mut other_grant = session_bootstrap(&pair._testnet, "replacement-withdrawal.test")
        .sign_in(identity, paykit_sdk::PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    other_grant.paykit_identity_secret_key = Some(replacement.clone());
    let replacement_sdk = PaykitSdk::new(
        pair.alice.storage.clone(),
        TestnetSessionProvider::new(other_grant),
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    replacement_sdk.initialize().await.unwrap();
    let retained = pair.alice.storage.snapshot().unwrap();
    assert!(replacement_sdk
        .finish_private_payment_list_withdrawal(original.clone())
        .await
        .is_err());
    assert_eq!(pair.alice.storage.snapshot().unwrap(), retained);

    let mut original_access = pair.alice.access.clone();
    original_access.paykit_identity_secret_key = Some(replacement);
    let original_sdk = PaykitSdk::new(
        pair.alice.storage.clone(),
        TestnetSessionProvider::new(original_access),
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    original_sdk.initialize().await.unwrap();
    assert!(original_sdk
        .retry_private_payment_list_withdrawal(original.clone())
        .await
        .is_err());
    assert!(original_sdk.export_backup_state().await.is_err());
    original_sdk
        .finish_private_payment_list_withdrawal(original.clone())
        .await
        .unwrap();
    assert!(original_sdk.export_backup_state().await.is_ok());
    let finished = pair.alice.storage.snapshot().unwrap();
    assert!(finished.private_payment_list_withdrawals.is_empty());
    assert_eq!(
        finished
            .outbound_private_messages
            .iter()
            .find(|m| m.outbound_message_id == original.outbound_message_id)
            .unwrap()
            .status,
        OutboundPrivateMessageStatus::Sent
    );
}

#[tokio::test]
async fn test_withdrawal_survives_restart_without_restoring_blocked_transport() {
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    let old = pair
        .alice
        .sdk
        .enqueue_private_payment_list_with_receiving_details(
            peer.clone(),
            vec![private_receiving_detail(
                "btc-lightning-bolt11",
                "old-private",
            )],
        )
        .await
        .unwrap();
    pair.alice
        .sdk
        .process_outbound_private_messages(peer.clone())
        .await
        .unwrap();
    pair.bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap();
    let id = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .unwrap();
    let state = pair.alice.storage.snapshot().unwrap();
    assert_eq!(state.linked_peers[&peer].state, LinkedPeerState::Blocked);
    assert!(state.encrypted_link_states[&peer].link_snapshot.is_none());
    assert!(state.encrypted_link_states[&peer]
        .handshake_snapshot
        .is_none());
    assert_eq!(state.private_payment_list_withdrawals.len(), 1);
    assert!(pair.alice.sdk.export_backup_state().await.is_err());
    assert!(pair
        .alice
        .sdk
        .finish_private_payment_list_withdrawal(id.clone())
        .await
        .is_err());
    let bytes = encode_storage_state_blob(&state).unwrap();
    let restarted = pair
        .alice
        .restart_with_storage(paykit_sdk::InMemoryStorage::from_state(
            decode_storage_state_blob(&bytes).unwrap(),
        ))
        .await;
    assert_eq!(
        restarted
            .sdk
            .prepare_private_payment_list_withdrawal_and_block(peer.clone())
            .await
            .unwrap(),
        id
    );
    assert!(restarted
        .sdk
        .clear_private_payment_list(peer.clone())
        .await
        .is_err());
    assert!(restarted
        .sdk
        .retry_private_payment_list_withdrawal(paykit_sdk::PrivatePaymentListWithdrawal {
            outbound_message_id: id.outbound_message_id + 1,
            ..id.clone()
        })
        .await
        .is_err());
    let ack = restarted
        .sdk
        .retry_private_payment_list_withdrawal(id.clone())
        .await
        .unwrap();
    assert_eq!(ack.withdrawal.counterparty, peer);
    assert_eq!(ack.withdrawal, id);
    assert_eq!(
        restarted
            .sdk
            .retry_private_payment_list_withdrawal(id.clone())
            .await
            .unwrap(),
        ack
    );
    pair.bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap();
    let views = pair
        .bob
        .sdk
        .current_private_payment_lists(&pair.alice.public_key)
        .await
        .unwrap();
    assert!(views
        .iter()
        .find(|v| v.app_id == pair.alice.app_id)
        .unwrap()
        .payment_endpoints
        .is_empty());
    let after = restarted.storage.snapshot().unwrap();
    assert_eq!(after.linked_peers[&peer].state, LinkedPeerState::Blocked);
    assert!(after.encrypted_link_states[&peer].link_snapshot.is_none());
    assert_eq!(
        after
            .outbound_private_messages
            .iter()
            .find(|m| m.outbound_message_id == old.outbound_message_id)
            .unwrap()
            .status,
        OutboundPrivateMessageStatus::Sent
    );
    restarted
        .sdk
        .finish_private_payment_list_withdrawal(id.clone())
        .await
        .unwrap();
    assert!(restarted
        .storage
        .snapshot()
        .unwrap()
        .private_payment_list_withdrawals
        .is_empty());
    assert!(restarted.sdk.export_backup_state().await.is_ok());
    assert!(restarted
        .sdk
        .retry_private_payment_list_withdrawal(id.clone())
        .await
        .is_err());
}

async fn reserve_packet(
    user: &TestUser,
    peer: &TestUser,
    empty: bool,
) -> (u64, PreparedOutboundPrivateSend) {
    let details = if empty {
        Vec::new()
    } else {
        vec![private_receiving_detail(
            "btc-lightning-bolt11",
            "occupied-private",
        )]
    };
    let message = user
        .sdk
        .enqueue_private_payment_list_with_receiving_details(peer.public_key.clone(), details)
        .await
        .unwrap();
    let state = user.storage.snapshot().unwrap();
    let original = state.encrypted_link_states[&peer.public_key].clone();
    let snapshot =
        paykit_lib::EncryptedLinkSnapshot::deserialize(original.link_snapshot.as_ref().unwrap())
            .unwrap();
    let secret = user
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(1)
        .unwrap();
    // Match the original SDK's generation-one Noise derivation without replacing
    // the link or allocating a different session/grant.
    let mut link = paykit_lib::restore_encrypted_link(
        user.access.session.clone(),
        paykit_lib::derive_paykit_noise_secret_key(secret.as_bytes()),
        &peer.public_key.to_public_key().unwrap(),
        user.access.outbox_client.clone(),
        snapshot,
    )
    .await
    .unwrap();
    let packet = link
        .prepare_private_application_message_json(&message.raw_json)
        .unwrap();
    let prepared = PreparedOutboundPrivateSend {
        destination_path: packet.destination_path().to_owned(),
        ciphertext: packet.ciphertext().to_vec(),
    };
    user.storage
        .transaction(|tx| {
            let mut message = message.clone();
            message.status = OutboundPrivateMessageStatus::Sending;
            message.last_attempt_at = Some(chrono::Utc::now() - chrono::Duration::hours(1));
            message.attempt_count = 1;
            message.prepared_send = Some(prepared.clone());
            tx.save_outbound_private_message(message)?;
            tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                link_snapshot: Some(packet.resulting_snapshot().serialize()),
                generation: original.generation + 1,
                ..original.clone()
            });
            tx.rebase_buffered_private_intake_generation(
                &peer.public_key,
                original.generation,
                original.generation + 1,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (message.outbound_message_id, prepared)
}

#[tokio::test]
async fn test_withdrawal_refuses_occupied_nonempty_slot_and_reuses_exact_empty() {
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    let (_, _) = reserve_packet(&pair.alice, &pair.bob, false).await;
    let before = pair.alice.storage.snapshot().unwrap();
    assert!(pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .is_err());
    let after = pair.alice.storage.snapshot().unwrap();
    assert_eq!(after.encrypted_link_states, before.encrypted_link_states);
    assert_eq!(
        after.outbound_private_messages,
        before.outbound_private_messages
    );
    assert!(after.private_payment_list_withdrawals.is_empty());
    // Finish the original publication normally; no slot is skipped by withdrawal.
    pair.alice
        .sdk
        .process_outbound_private_messages(peer.clone())
        .await
        .unwrap();
    pair.bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap();
    let (id, packet) = reserve_packet(&pair.alice, &pair.bob, true).await;
    let handle = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .unwrap();
    assert_eq!(handle.outbound_message_id, id);
    pair.alice
        .sdk
        .retry_private_payment_list_withdrawal(handle)
        .await
        .unwrap();
    let actual = pair
        .alice
        .access
        .session
        .storage()
        .get(&packet.destination_path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(actual.as_ref(), packet.ciphertext.as_slice());
}

#[tokio::test]
async fn test_withdrawal_refuses_already_lost_state_and_block_generation_aba() {
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    pair.alice.sdk.block_peer(peer.clone()).await.unwrap();
    assert!(pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .is_err());
    assert!(pair
        .alice
        .storage
        .snapshot()
        .unwrap()
        .private_payment_list_withdrawals
        .is_empty());
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    let id = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .unwrap();
    pair.alice.sdk.unblock_peer(peer.clone()).await.unwrap();
    pair.alice.sdk.block_peer(peer.clone()).await.unwrap();
    assert!(pair
        .alice
        .sdk
        .retry_private_payment_list_withdrawal(id.clone())
        .await
        .is_err());
    assert!(pair
        .alice
        .sdk
        .finish_private_payment_list_withdrawal(id.clone())
        .await
        .is_err());
    assert!(pair.alice.sdk.export_backup_state().await.is_err());
}

#[tokio::test]
async fn test_withdrawal_rejects_foreign_context_and_wrong_original_stream() {
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    let (id, packet) = reserve_packet(&pair.alice, &pair.bob, true).await;
    pair.alice
        .storage
        .transaction(|tx| {
            let mut message = tx
                .outbound_private_messages(&peer)
                .into_iter()
                .find(|m| m.outbound_message_id == id)
                .unwrap();
            let path = &mut message.prepared_send.as_mut().unwrap().destination_path;
            *path = format!(
                "{}/{}/0",
                paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX,
                "ab".repeat(32)
            );
            tx.save_outbound_private_message(message)
        })
        .await
        .unwrap();
    assert!(pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .is_err());
    assert_eq!(
        pair.alice.storage.snapshot().unwrap().linked_peers[&peer].state,
        LinkedPeerState::Linked
    );
    assert!(pair
        .alice
        .storage
        .snapshot()
        .unwrap()
        .private_payment_list_withdrawals
        .is_empty());
    pair.alice
        .storage
        .transaction(|tx| {
            let mut message = tx
                .outbound_private_messages(&peer)
                .into_iter()
                .find(|m| m.outbound_message_id == id)
                .unwrap();
            message.prepared_send = Some(packet.clone());
            tx.save_outbound_private_message(message)
        })
        .await
        .unwrap();
    let original = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .unwrap();
    let mut variants = Vec::new();
    let mut wrong = original.clone();
    wrong.owner = peer.clone();
    variants.push(wrong);
    let mut wrong = original.clone();
    wrong.app_id = app_id("foreign");
    variants.push(wrong);
    let mut wrong = original.clone();
    wrong.original_binding = "another-original-grant".into();
    variants.push(wrong);
    for wrong in variants {
        assert!(pair
            .alice
            .sdk
            .retry_private_payment_list_withdrawal(wrong.clone())
            .await
            .is_err());
        assert!(pair
            .alice
            .sdk
            .finish_private_payment_list_withdrawal(wrong)
            .await
            .is_err());
    }
    assert!(pair
        .alice
        .access
        .session
        .storage()
        .get(&packet.destination_path)
        .await
        .is_err());
    let ack = pair
        .alice
        .sdk
        .retry_private_payment_list_withdrawal(original.clone())
        .await
        .unwrap();
    assert_eq!(ack.withdrawal, original);
}

use paykit_sdk::storage::StorageTransactionCallback;
use paykit_sdk::{Clock, InMemoryStorage, PaykitSdk, PaykitSdkConfig, PaykitSdkError};
use std::{
    any::Any,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::oneshot;

type AttemptPause = (oneshot::Sender<()>, oneshot::Receiver<()>);

#[derive(Clone)]
struct ControlledStorage {
    inner: InMemoryStorage,
    fail_ack: Arc<AtomicBool>,
    pause_attempt: Arc<Mutex<Option<AttemptPause>>>,
}

impl ControlledStorage {
    fn new(inner: InMemoryStorage) -> Self {
        Self {
            inner,
            fail_ack: Arc::new(AtomicBool::new(false)),
            pause_attempt: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait::async_trait]
impl StorageAdapter for ControlledStorage {
    async fn transaction_erased<'a>(
        &self,
        operation: StorageTransactionCallback<'a>,
    ) -> paykit_sdk::Result<Box<dyn Any + Send>> {
        let mut attempt_started = false;
        let result = self
            .inner
            .transaction_erased(Box::new(|tx| {
                let before = tx.export_storage_state();
                let result = operation(tx)?;
                let after = tx.export_storage_state();
                if !before.private_payment_list_withdrawals.is_empty() {
                    attempt_started = after.outbound_private_messages.iter().any(|m| {
                        before.outbound_private_messages.iter().any(|old| {
                            old.outbound_message_id == m.outbound_message_id
                                && m.attempt_count > old.attempt_count
                        })
                    });
                    let acknowledged = after.outbound_private_messages.iter().any(|m| {
                        m.status == OutboundPrivateMessageStatus::Sent
                            && before.outbound_private_messages.iter().any(|old| {
                                old.outbound_message_id == m.outbound_message_id
                                    && old.status != OutboundPrivateMessageStatus::Sent
                            })
                    });
                    if acknowledged && self.fail_ack.swap(false, Ordering::SeqCst) {
                        return Err(PaykitSdkError::Storage {
                            context: "injected acknowledgement commit failure".into(),
                            source: None,
                        });
                    }
                }
                Ok(result)
            }))
            .await;
        if attempt_started {
            let pause = self.pause_attempt.lock().unwrap().take();
            if let Some((entered, release)) = pause {
                let _ = entered.send(());
                let _ = release.await;
            }
        }
        result
    }
}

#[derive(Clone)]
struct WithdrawalClock(Arc<Mutex<chrono::DateTime<chrono::Utc>>>);
impl Clock for WithdrawalClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        *self.0.lock().unwrap()
    }
}

type ControlledSdk =
    PaykitSdk<ControlledStorage, TestnetSessionProvider, TestnetPaymentAdapter, WithdrawalClock>;
fn controlled_sdk(user: &TestUser) -> (Arc<ControlledSdk>, ControlledStorage, WithdrawalClock) {
    let storage = ControlledStorage::new(user.storage.clone());
    let clock = WithdrawalClock(Arc::new(Mutex::new(chrono::Utc::now())));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestnetSessionProvider::with_session_secret(
            user.access.clone(),
            user.session_secret.clone(),
        ),
        user.adapter.clone(),
        PaykitSdkConfig::new(user.app_id.clone()).unwrap(),
        clock.clone(),
    );
    (Arc::new(sdk), storage, clock)
}

#[tokio::test]
async fn test_withdrawal_keeps_exact_packet_after_publication_commit_failure() {
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    let (_, packet) = reserve_packet(&pair.alice, &pair.bob, true).await;
    let original = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .unwrap();
    let (sdk, storage, _) = controlled_sdk(&pair.alice);
    storage.fail_ack.store(true, Ordering::SeqCst);
    assert!(sdk
        .retry_private_payment_list_withdrawal(original.clone())
        .await
        .is_err());
    assert!(sdk
        .finish_private_payment_list_withdrawal(original.clone())
        .await
        .is_err());
    assert!(sdk.export_backup_state().await.is_err());
    let actual = pair
        .alice
        .access
        .session
        .storage()
        .get(&packet.destination_path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(actual.as_ref(), packet.ciphertext.as_slice());
    let snapshot = encode_storage_state_blob(&pair.alice.storage.snapshot().unwrap()).unwrap();
    let restarted = pair
        .alice
        .restart_with_storage(InMemoryStorage::from_state(
            decode_storage_state_blob(&snapshot).unwrap(),
        ))
        .await;
    let ack = restarted
        .sdk
        .retry_private_payment_list_withdrawal(original.clone())
        .await
        .unwrap();
    assert_eq!(ack.withdrawal, original);
    let repeated = pair
        .alice
        .access
        .session
        .storage()
        .get(&packet.destination_path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(actual, repeated);
    pair.bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap();
    let views = pair
        .bob
        .sdk
        .current_private_payment_lists(&pair.alice.public_key)
        .await
        .unwrap();
    assert!(views
        .iter()
        .find(|v| v.app_id == pair.alice.app_id)
        .unwrap()
        .payment_endpoints
        .is_empty());
}

#[tokio::test]
async fn test_withdrawal_rechecks_lease_after_suspended_admission() {
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    let (_, packet) = reserve_packet(&pair.alice, &pair.bob, true).await;
    let original = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .unwrap();
    let (sdk, storage, clock) = controlled_sdk(&pair.alice);
    let (entered, waiting) = oneshot::channel();
    let (release, released) = oneshot::channel();
    *storage.pause_attempt.lock().unwrap() = Some((entered, released));
    let task = tokio::spawn({
        let sdk = sdk.clone();
        let original = original.clone();
        async move { sdk.retry_private_payment_list_withdrawal(original).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap();
    *clock.0.lock().unwrap() += chrono::Duration::hours(1);
    let replacement = PaykitSdk::with_clock(
        pair.alice.storage.clone(),
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
        clock,
    );
    replacement.unblock_peer(peer.clone()).await.unwrap();
    replacement.block_peer(peer.clone()).await.unwrap();
    release.send(()).unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(pair
        .alice
        .access
        .session
        .storage()
        .get(&packet.destination_path)
        .await
        .is_err());
    assert!(sdk
        .finish_private_payment_list_withdrawal(original)
        .await
        .is_err());
}
