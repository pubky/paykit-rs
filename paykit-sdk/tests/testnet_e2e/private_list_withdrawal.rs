use super::harness::*;
use paykit_sdk::storage::{
    decode_storage_state_blob, encode_storage_state_blob, EncryptedLinkStateRecord,
    PreparedOutboundPrivateSend,
};
use paykit_sdk::{LinkedPeerState, OutboundPrivateMessageStatus, StorageAdapter};

#[tokio::test]
async fn test_withdrawal_blocks_app_removal_until_confirmed() {
    let pair = linked_two_party().await;
    let withdrawal = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(pair.bob.public_key.clone())
        .await
        .unwrap();
    let retained = pair.alice.storage.snapshot().unwrap();
    let mut v5 = encode_storage_state_blob(&retained).unwrap();
    v5[0] = 5;
    assert_eq!(v5.pop(), Some(0)); // v6's empty denial set follows the v5 fields.
    assert_eq!(decode_storage_state_blob(&v5).unwrap(), retained);
    v5.push(0);
    assert!(decode_storage_state_blob(&v5).is_err());
    assert!(matches!(pair.alice.sdk.remove_paykit_app().await,
        Err(paykit_sdk::PaykitSdkError::Policy { context, .. })
        if context.contains("unconfirmed private list withdrawal")));
    let after_refusal = pair.alice.storage.snapshot().unwrap();
    assert_eq!(
        after_refusal.private_payment_list_withdrawals,
        retained.private_payment_list_withdrawals
    );
    assert_eq!(
        after_refusal.outbound_private_messages,
        retained.outbound_private_messages
    );
    assert!(!after_refusal
        .retired_paykit_apps
        .contains(&pair.alice.app_id));

    let ack = pair
        .alice
        .sdk
        .retry_private_payment_list_withdrawal(withdrawal.clone())
        .await
        .unwrap();
    assert_eq!(ack.withdrawal, withdrawal);
    // A confirmed receipt remains finishable after the app's registry removal.
    let registry = pair.alice.sdk.remove_paykit_app().await.unwrap();
    assert!(!registry.apps().contains_key(&pair.alice.app_id));
    pair.alice
        .sdk
        .finish_private_payment_list_withdrawal(withdrawal)
        .await
        .unwrap();
    assert!(pair.alice.sdk.export_backup_state().await.is_ok());
}

#[tokio::test]
async fn test_withdrawal_blocks_rotation_until_confirmed() {
    let pair = linked_two_party().await;
    let withdrawal = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(pair.bob.public_key.clone())
        .await
        .unwrap();
    let retained = pair.alice.storage.snapshot().unwrap();
    let identity = pair.alice.access.local_secret_key.as_ref().unwrap();
    let replacement = identity.derive_paykit_identity_secret_key(2).unwrap();
    assert!(
        matches!(pair.alice.sdk.rotate_paykit_identity_key(replacement).await,
        Err(paykit_sdk::PaykitSdkError::Policy { context, .. })
        if context.contains("unconfirmed private list withdrawal"))
    );
    assert_eq!(pair.alice.storage.snapshot().unwrap(), retained);
    let ack = pair
        .alice
        .sdk
        .retry_private_payment_list_withdrawal(withdrawal.clone())
        .await
        .unwrap();
    assert_eq!(ack.withdrawal, withdrawal);
    pair.alice
        .sdk
        .finish_private_payment_list_withdrawal(withdrawal)
        .await
        .unwrap();
    assert!(pair.alice.sdk.export_backup_state().await.is_ok());
}

#[tokio::test]
async fn test_withdrawal_blocks_original_grant_sign_out_until_finished() {
    let pair = linked_two_party().await;
    let withdrawal = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(pair.bob.public_key.clone())
        .await
        .unwrap();
    let retained = pair.alice.storage.snapshot().unwrap();
    assert!(matches!(pair.alice.sdk.sign_out().await,
        Err(paykit_sdk::PaykitSdkError::Policy { context, .. })
        if context.contains("before finishing this grant's private list withdrawals")));
    assert_eq!(pair.alice.storage.snapshot().unwrap(), retained);

    // A different grant for the same owner does not own this retained operation.
    let other = session_bootstrap(&pair._testnet, "paykit-sdk.test")
        .sign_in(
            pair.alice.access.local_secret_key.as_ref().unwrap(),
            paykit_sdk::PAYKIT_SESSION_CAPABILITIES,
        )
        .await
        .unwrap();
    let other_secret = other.export_session_secret().await.unwrap().into_inner();
    let other_sdk = PaykitSdk::new(
        pair.alice.storage.clone(),
        TestnetSessionProvider::with_session_secret(other.access, other_secret),
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    other_sdk.initialize().await.unwrap();
    other_sdk.sign_out().await.unwrap();
    let ack = pair
        .alice
        .sdk
        .retry_private_payment_list_withdrawal(withdrawal.clone())
        .await
        .unwrap();
    assert_eq!(ack.withdrawal, withdrawal);
    let confirmed = pair.alice.storage.snapshot().unwrap();
    assert!(matches!(pair.alice.sdk.sign_out().await,
        Err(paykit_sdk::PaykitSdkError::Policy { context, .. })
        if context.contains("before finishing this grant's private list withdrawals")));
    assert_eq!(pair.alice.storage.snapshot().unwrap(), confirmed);
    pair.alice
        .sdk
        .finish_private_payment_list_withdrawal(withdrawal)
        .await
        .unwrap();
    assert!(pair.alice.sdk.export_backup_state().await.is_ok());
    pair.alice.sdk.sign_out().await.unwrap();
    assert!(pair
        .alice
        .sdk
        .retry_private_payment_list_withdrawal(ack.withdrawal)
        .await
        .is_err());
}

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
async fn test_withdrawal_from_older_link_state_preserves_used_slot() {
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    let older = pair.alice.storage.snapshot().unwrap();
    let (sent_id, first) = reserve_packet(&pair.alice, &pair.bob, false).await;
    let report = pair
        .alice
        .sdk
        .process_outbound_private_messages(peer.clone())
        .await
        .unwrap();
    assert_eq!(report.sent, vec![sent_id]);
    let restored = pair
        .alice
        .restart_with_storage(InMemoryStorage::from_state(older))
        .await;
    let handle = restored
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .unwrap();
    // Inspect the serialized storage record through the public adapter boundary.
    #[derive(serde::Deserialize)]
    struct StoredWithdrawal {
        prepared: PreparedOutboundPrivateSend,
    }
    let state = restored.storage.snapshot().unwrap();
    let record = serde_json::to_value(&state.private_payment_list_withdrawals[&peer]).unwrap();
    let journal: StoredWithdrawal = serde_json::from_value(record).unwrap();
    let (stream, slot) = first.destination_path.rsplit_once('/').unwrap();
    assert_eq!(
        journal.prepared.destination_path,
        format!("{stream}/{}", slot.parse::<u32>().unwrap() + 1)
    );
    let ack = restored
        .sdk
        .retry_private_payment_list_withdrawal(handle.clone())
        .await
        .unwrap();
    assert_eq!(ack.withdrawal, handle);
    for packet in [first, journal.prepared] {
        let stored = restored
            .access
            .session
            .storage()
            .get(&packet.destination_path)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(stored.as_ref(), packet.ciphertext.as_slice());
    }
}

#[tokio::test]
async fn test_withdrawal_refuses_another_packet_before_and_after_block() {
    // The same occupied slot is observed at two different authority boundaries.
    for already_blocked in [false, true] {
        let pair = linked_two_party().await;
        let peer = pair.bob.public_key.clone();
        let (_, packet) = reserve_packet(&pair.alice, &pair.bob, true).await;
        let sdk = &pair.alice.sdk;
        let handle = if already_blocked {
            Some(
                sdk.prepare_private_payment_list_withdrawal_and_block(peer.clone())
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let mut foreign = packet.ciphertext.clone();
        foreign[0] ^= 1;
        let storage = pair.alice.access.session.storage();
        storage
            .put(&packet.destination_path, foreign.clone())
            .await
            .unwrap();
        let before = pair.alice.storage.snapshot().unwrap();
        if let Some(handle) = handle {
            assert!(sdk
                .retry_private_payment_list_withdrawal(handle.clone())
                .await
                .is_err());
            assert!(sdk
                .finish_private_payment_list_withdrawal(handle)
                .await
                .is_err());
            assert!(sdk.export_backup_state().await.is_err());
        } else {
            assert!(sdk
                .prepare_private_payment_list_withdrawal_and_block(peer.clone())
                .await
                .is_err());
        }
        let after = pair.alice.storage.snapshot().unwrap();
        assert_eq!(after.linked_peers, before.linked_peers);
        assert_eq!(after.encrypted_link_states, before.encrypted_link_states);
        assert_eq!(
            after.private_payment_list_withdrawals,
            before.private_payment_list_withdrawals
        );
        let actual = storage
            .get(&packet.destination_path)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(actual.as_ref(), foreign.as_slice());
    }
}

#[tokio::test]
async fn test_repeated_block_and_contact_removal_preserve_original_withdrawal() {
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    pair.alice
        .sdk
        .save_contact(paykit_sdk::ContactUpdate {
            public_key: peer.clone(),
            label: None,
        })
        .await
        .unwrap();
    let original = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .unwrap();
    let retained = pair.alice.storage.snapshot().unwrap();

    let blocked = pair.alice.sdk.block_peer(peer.clone()).await.unwrap();
    assert_eq!(blocked.state, LinkedPeerState::Blocked);
    assert_eq!(
        pair.alice
            .storage
            .snapshot()
            .unwrap()
            .private_payment_list_withdrawals,
        retained.private_payment_list_withdrawals
    );
    let removed = pair
        .alice
        .sdk
        .remove_contacts_and_block_peers(vec![peer.clone()])
        .await
        .unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].public_key, peer);
    let after = pair.alice.storage.snapshot().unwrap();
    assert_eq!(after.linked_peers[&peer].state, LinkedPeerState::Blocked);
    assert_eq!(
        after.encrypted_link_states[&peer],
        retained.encrypted_link_states[&peer]
    );
    assert_eq!(
        after.private_payment_list_withdrawals,
        retained.private_payment_list_withdrawals
    );
    assert!(pair.alice.sdk.export_backup_state().await.is_err());

    let ack = pair
        .alice
        .sdk
        .retry_private_payment_list_withdrawal(original.clone())
        .await
        .unwrap();
    assert_eq!(ack.withdrawal, original);
    pair.alice
        .sdk
        .finish_private_payment_list_withdrawal(original)
        .await
        .unwrap();
    assert!(pair.alice.sdk.export_backup_state().await.is_ok());
}

#[tokio::test]
async fn test_repeated_block_refuses_conflicting_prepared_send_before_parking() {
    let pair = linked_two_party().await;
    let peer = pair.bob.public_key.clone();
    let (id, packet) = reserve_packet(&pair.alice, &pair.bob, true).await;
    let withdrawal = pair
        .alice
        .sdk
        .prepare_private_payment_list_withdrawal_and_block(peer.clone())
        .await
        .unwrap();
    assert_eq!(withdrawal.outbound_message_id, id);

    // An inconsistent adapter supplies both a live prepared slot and the blocked journal.
    pair.alice
        .storage
        .transaction(|tx| {
            let mut row = tx
                .outbound_private_messages(&peer)
                .into_iter()
                .find(|r| r.outbound_message_id == id)
                .unwrap();
            row.status = OutboundPrivateMessageStatus::Sending;
            row.last_error = None;
            row.prepared_send = Some(packet.clone());
            tx.save_outbound_private_message(row)
        })
        .await
        .unwrap();
    let before = pair.alice.storage.snapshot().unwrap();
    assert!(paykit_sdk::validate_storage_state(&before).is_err());
    assert!(pair.alice.sdk.block_peer(peer).await.is_err());
    assert_eq!(pair.alice.storage.snapshot().unwrap(), before);
    assert!(pair
        .alice
        .sdk
        .retry_private_payment_list_withdrawal(withdrawal)
        .await
        .is_err());
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
    pause_prepare: Arc<Mutex<Option<AttemptPause>>>,
}

impl ControlledStorage {
    fn new(inner: InMemoryStorage) -> Self {
        Self {
            inner,
            fail_ack: Arc::new(AtomicBool::new(false)),
            pause_attempt: Arc::new(Mutex::new(None)),
            pause_prepare: Arc::new(Mutex::new(None)),
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
        let mut prepare_admitted = false;
        let result = self
            .inner
            .transaction_erased(Box::new(|tx| {
                let before = tx.export_storage_state();
                let result = operation(tx)?;
                let after = tx.export_storage_state();
                prepare_admitted = before == after
                    && before.private_payment_list_withdrawals.is_empty()
                    && !before.peer_link_operation_leases.is_empty()
                    && !before.paykit_app_operation_leases.is_empty();
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
        if prepare_admitted {
            let pause = self.pause_prepare.lock().unwrap().take();
            if let Some((entered, release)) = pause {
                let _ = entered.send(());
                let _ = release.await;
            }
        }
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

#[derive(Clone)]
struct HeldRevocation {
    inner: TestnetSessionProvider,
    pause: Arc<Mutex<Option<AttemptPause>>>,
}

#[async_trait::async_trait]
impl paykit_sdk::PubkySessionProvider for HeldRevocation {
    async fn load_session_access(
        &self,
    ) -> paykit_sdk::Result<Option<paykit_sdk::PubkySessionAccess>> {
        self.inner.load_session_access().await
    }

    async fn revoke_session_access(
        &self,
        access: &paykit_sdk::PubkySessionAccess,
    ) -> paykit_sdk::Result<()> {
        let pause = self.pause.lock().unwrap().take();
        if let Some((entered, release)) = pause {
            let _ = entered.send(());
            let _ = release.await;
        }
        self.inner.revoke_session_access(access).await
    }

    async fn load_public_storage(&self) -> paykit_sdk::Result<Option<pubky::PublicStorage>> {
        self.inner.load_public_storage().await
    }

    async fn clear_session_access(&self) -> paykit_sdk::Result<()> {
        self.inner.clear_session_access().await
    }
}

fn held_sign_out(
    user: &TestUser,
) -> (
    tokio::task::JoinHandle<paykit_sdk::Result<paykit_sdk::IdentityStatus>>,
    oneshot::Receiver<()>,
    oneshot::Sender<()>,
) {
    let (entered, waiting) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let provider = HeldRevocation {
        inner: TestnetSessionProvider::with_session_secret(
            user.access.clone(),
            user.session_secret.clone(),
        ),
        pause: Arc::new(Mutex::new(Some((entered, released)))),
    };
    let sdk = PaykitSdk::new(
        user.storage.clone(),
        provider,
        user.adapter.clone(),
        PaykitSdkConfig::new(user.app_id.clone()).unwrap(),
    );
    (
        tokio::spawn(async move { sdk.sign_out().await }),
        waiting,
        release,
    )
}

async fn wait_for_admission(waiting: oneshot::Receiver<()>) {
    tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap();
}

fn assert_no_withdrawal_prepared(user: &TestUser, before: &paykit_sdk::storage::StorageState) {
    let after = user.storage.snapshot().unwrap();
    assert!(after.private_payment_list_withdrawals.is_empty());
    assert_eq!(after.linked_peers, before.linked_peers);
    assert_eq!(
        after.outbound_private_messages,
        before.outbound_private_messages
    );
    assert_eq!(after.encrypted_link_states, before.encrypted_link_states);
    assert_eq!(after.closed_private_payment_list_withdrawal_grants.len(), 1);
}

#[tokio::test]
async fn test_withdrawal_sign_out_fences_other_instance_and_survives_cancellation() {
    let pair = linked_two_party().await;
    let before = pair.alice.storage.snapshot().unwrap();
    let (sign_out, entered, _release) = held_sign_out(&pair.alice);
    wait_for_admission(entered).await;
    assert!(matches!(
        pair.alice
            .sdk
            .prepare_private_payment_list_withdrawal_and_block(pair.bob.public_key.clone())
            .await,
        Err(PaykitSdkError::Policy { .. })
    ));
    assert_no_withdrawal_prepared(&pair.alice, &before);
    sign_out.abort();
    assert!(sign_out.await.unwrap_err().is_cancelled());

    // A process restart preserves the denial, without inventing a revoke ACK.
    let state = pair.alice.storage.snapshot().unwrap();
    let blob = encode_storage_state_blob(&state).unwrap();
    let restored_state = decode_storage_state_blob(&blob).unwrap();
    assert_eq!(restored_state, state);
    let restarted = pair
        .alice
        .restart_with_storage(InMemoryStorage::from_state(restored_state))
        .await;
    assert!(matches!(
        restarted
            .sdk
            .prepare_private_payment_list_withdrawal_and_block(pair.bob.public_key.clone())
            .await,
        Err(PaykitSdkError::Policy { .. })
    ));
    assert!(restarted.sdk.export_backup_state().await.is_ok());

    // Retry of the original sign-out remains available; a new grant is distinct.
    restarted.sdk.sign_out().await.unwrap();
    let fresh = session_bootstrap(&pair._testnet, "paykit-sdk.test")
        .sign_in(
            pair.alice.access.local_secret_key.as_ref().unwrap(),
            paykit_sdk::PAYKIT_SESSION_CAPABILITIES,
        )
        .await
        .unwrap();
    let fresh_sdk = PaykitSdk::new(
        restarted.storage.clone(),
        TestnetSessionProvider::new(fresh.access),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    fresh_sdk.initialize().await.unwrap();
    let withdrawal = fresh_sdk
        .prepare_private_payment_list_withdrawal_and_block(pair.bob.public_key.clone())
        .await
        .unwrap();
    fresh_sdk
        .retry_private_payment_list_withdrawal(withdrawal.clone())
        .await
        .unwrap();
    fresh_sdk
        .finish_private_payment_list_withdrawal(withdrawal)
        .await
        .unwrap();

    // Denial-only history also works before initialize; failed revoke retains it.
    let empty_storage = InMemoryStorage::new();
    let uninitialized = PaykitSdk::new(
        empty_storage.clone(),
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    assert!(uninitialized.sign_out().await.is_err());
    let empty = empty_storage.snapshot().unwrap();
    assert!(empty.identity_state.is_none());
    assert_eq!(empty.closed_private_payment_list_withdrawal_grants.len(), 1);
    assert_eq!(
        decode_storage_state_blob(&encode_storage_state_blob(&empty).unwrap()).unwrap(),
        empty
    );
    assert!(uninitialized.export_backup_state().await.is_ok());
}

#[tokio::test]
async fn test_withdrawal_sign_out_fences_previously_admitted_preparation() {
    let pair = linked_two_party().await;
    let before = pair.alice.storage.snapshot().unwrap();
    let (sdk, storage, _) = controlled_sdk(&pair.alice);
    let (entered, waiting) = oneshot::channel();
    let (release_prepare, released) = oneshot::channel();
    *storage.pause_prepare.lock().unwrap() = Some((entered, released));
    let prepare = tokio::spawn({
        let peer = pair.bob.public_key.clone();
        async move {
            sdk.prepare_private_payment_list_withdrawal_and_block(peer)
                .await
        }
    });
    wait_for_admission(waiting).await;
    let (sign_out, entered, release_sign_out) = held_sign_out(&pair.alice);
    wait_for_admission(entered).await;
    release_prepare.send(()).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), prepare)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert_no_withdrawal_prepared(&pair.alice, &before);
    release_sign_out.send(()).unwrap();
    sign_out.await.unwrap().unwrap();
    assert_eq!(
        pair.alice
            .storage
            .snapshot()
            .unwrap()
            .closed_private_payment_list_withdrawal_grants
            .len(),
        1
    );
}
