use paykit_lib::{AllowanceAmountRange, AllowanceId, AllowanceTerms, PrivateMessageKind};
use paykit_sdk::{
    AllowanceHistoryStatus, AllowanceLifecycleState, AllowanceLocalRole, InMemoryStorage,
    PaykitSdkError, PrivateStreamParseStatus,
};

use crate::harness::{
    drive_recovery_to_linked, linked_two_party,
    wait_until_marker_is_newer_than_observer_checkpoint, TestUser,
};

fn terms() -> AllowanceTerms {
    AllowanceTerms::builder("btc")
        .per_payment_amount(AllowanceAmountRange::new("0.0001", "0.01").unwrap())
        .lifetime_amount_limit("0.10")
        .build()
        .unwrap()
}

async fn deliver(sender: &TestUser, receiver: &TestUser) {
    let sent = sender
        .sdk
        .process_outbound_private_messages(
            receiver.public_key.clone(),
            receiver.receiver_path.clone(),
        )
        .await
        .expect("processing the Allowance outbound queue should succeed");
    assert!(!sent.sent.is_empty());
    assert!(sent.failed.is_empty());

    let received = receiver
        .sdk
        .receive_private_messages(sender.public_key.clone(), sender.receiver_path.clone())
        .await
        .expect("receiving Allowance messages should succeed");
    assert!(!received.stream_item_ids.is_empty());
    assert!(received.event_conflicts.is_empty());
}

async fn allowance(
    local: &TestUser,
    counterparty: &TestUser,
    allowance_id: &AllowanceId,
) -> paykit_sdk::AllowanceRecord {
    local
        .sdk
        .allowance_record(
            &counterparty.public_key,
            &counterparty.receiver_path,
            allowance_id,
        )
        .await
        .expect("Allowance lookup should succeed")
        .expect("Allowance should exist on this exact Encrypted Link")
}

#[tokio::test]
async fn test_allowance_lifecycle_roundtrip_between_linked_peers() {
    let pair = linked_two_party().await;

    let proposed = pair
        .alice
        .sdk
        .propose_allowance(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
            AllowanceLocalRole::Allower,
            terms(),
        )
        .await
        .expect("Alice should queue an Allowance proposal");
    let accepted_allowance_id = AllowanceId::new(&proposed.allowance_id).unwrap();
    assert_eq!(proposed.state, AllowanceLifecycleState::Proposed);
    assert_eq!(proposed.local_role, Some(AllowanceLocalRole::Allower));
    deliver(&pair.alice, &pair.bob).await;
    let bob_proposed = allowance(&pair.bob, &pair.alice, &accepted_allowance_id).await;
    assert_eq!(bob_proposed.state, AllowanceLifecycleState::Proposed);
    assert_eq!(bob_proposed.local_role, Some(AllowanceLocalRole::Allowee));

    pair.bob
        .sdk
        .accept_allowance(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
            &accepted_allowance_id,
        )
        .await
        .expect("Bob should queue the Allowance acceptance");
    deliver(&pair.bob, &pair.alice).await;
    let alice_accepted = allowance(&pair.alice, &pair.bob, &accepted_allowance_id).await;
    let bob_accepted = allowance(&pair.bob, &pair.alice, &accepted_allowance_id).await;
    for (record, expected_role) in [
        (&alice_accepted, AllowanceLocalRole::Allower),
        (&bob_accepted, AllowanceLocalRole::Allowee),
    ] {
        assert_eq!(record.state, AllowanceLifecycleState::Accepted);
        assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
        assert_eq!(record.local_role, Some(expected_role));
        assert!(record.acceptance_event_id.is_some());
    }
    assert_eq!(
        alice_accepted.acceptance_event_id,
        bob_accepted.acceptance_event_id
    );

    pair.alice
        .sdk
        .end_allowance(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
            &accepted_allowance_id,
        )
        .await
        .expect("Alice should queue the Allowance End");
    deliver(&pair.alice, &pair.bob).await;
    let alice_ended = allowance(&pair.alice, &pair.bob, &accepted_allowance_id).await;
    let bob_ended = allowance(&pair.bob, &pair.alice, &accepted_allowance_id).await;
    for (record, expected_role) in [
        (&alice_ended, AllowanceLocalRole::Allower),
        (&bob_ended, AllowanceLocalRole::Allowee),
    ] {
        assert_eq!(record.state, AllowanceLifecycleState::Ended);
        assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
        assert_eq!(record.local_role, Some(expected_role));
        assert!(record.end_event_id.is_some());
    }
    assert_eq!(alice_ended.end_event_id, bob_ended.end_event_id);

    let second_proposal = pair
        .bob
        .sdk
        .propose_allowance(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
            AllowanceLocalRole::Allowee,
            terms(),
        )
        .await
        .expect("Bob should queue a second Allowance proposal");
    let rejected_allowance_id = AllowanceId::new(&second_proposal.allowance_id).unwrap();
    deliver(&pair.bob, &pair.alice).await;
    pair.alice
        .sdk
        .reject_allowance(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
            &rejected_allowance_id,
        )
        .await
        .expect("Alice should queue the Allowance rejection");
    deliver(&pair.alice, &pair.bob).await;
    let alice_rejected = allowance(&pair.alice, &pair.bob, &rejected_allowance_id).await;
    let bob_rejected = allowance(&pair.bob, &pair.alice, &rejected_allowance_id).await;
    for (record, expected_role) in [
        (&alice_rejected, AllowanceLocalRole::Allower),
        (&bob_rejected, AllowanceLocalRole::Allowee),
    ] {
        assert_eq!(record.state, AllowanceLifecycleState::Rejected);
        assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
        assert_eq!(record.local_role, Some(expected_role));
        assert!(record.rejection_event_id.is_some());
    }
    assert_eq!(
        alice_rejected.rejection_event_id,
        bob_rejected.rejection_event_id
    );
}

#[tokio::test]
async fn test_allowance_survives_restart_legacy_replay_restore_and_link_recovery() {
    let pair = linked_two_party().await;
    let proposed = pair
        .alice
        .sdk
        .propose_allowance(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
            AllowanceLocalRole::Allower,
            terms(),
        )
        .await
        .expect("Alice should queue an Allowance proposal");
    let allowance_id = AllowanceId::new(&proposed.allowance_id).unwrap();
    let proposal_event_id = proposed
        .proposal_event_id
        .as_deref()
        .expect("proposal should retain its Event ID")
        .to_owned();
    deliver(&pair.alice, &pair.bob).await;

    let restarted_bob = pair
        .bob
        .restart_with_storage(pair.bob.storage.clone())
        .await;
    let restarted_record = allowance(&restarted_bob, &pair.alice, &allowance_id).await;
    assert_eq!(restarted_record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(
        restarted_record.history_status,
        AllowanceHistoryStatus::Consistent
    );

    let mut legacy_backup = restarted_bob
        .sdk
        .export_backup_state()
        .await
        .expect("Allowance backup export should succeed");
    let original_position = legacy_backup
        .private_stream_items
        .iter()
        .position(|item| {
            item.known_paykit_kind.as_deref()
                == Some(PrivateMessageKind::AllowanceProposal.as_str())
        })
        .expect("backup should contain the received Allowance proposal");
    let original_stream_item_id =
        legacy_backup.private_stream_items[original_position].stream_item_id;
    let replay_stream_item_id = legacy_backup.next_private_stream_item_id;
    let mut exact_replay = legacy_backup.private_stream_items[original_position].clone();
    exact_replay.stream_item_id = replay_stream_item_id;
    exact_replay.receive_batch_id = legacy_backup.next_receive_batch_id;

    // SECURITY: this deliberately emulates pre-Allowance private storage.
    // The raw plaintext is retained byte-for-byte and is never logged.
    for item in [
        &mut legacy_backup.private_stream_items[original_position],
        &mut exact_replay,
    ] {
        item.known_paykit_kind = None;
        item.parse_status = PrivateStreamParseStatus::UnknownKind;
        item.parse_error = None;
    }
    legacy_backup.private_stream_items.push(exact_replay);
    legacy_backup.next_private_stream_item_id = replay_stream_item_id + 1;
    legacy_backup.next_receive_batch_id += 1;
    legacy_backup
        .event_dedup_records
        .retain(|record| record.event_id != proposal_event_id);

    let restored_bob = restarted_bob
        .restart_with_storage(InMemoryStorage::new())
        .await;
    let restore = restored_bob
        .sdk
        .restore_backup_state(legacy_backup)
        .await
        .expect("legacy Allowance backup restore should succeed");
    assert!(restore.recovery_required_peers.is_empty());
    let migrated = restored_bob
        .sdk
        .export_backup_state()
        .await
        .expect("migrated Allowance backup export should succeed");
    let dedupe = migrated
        .event_dedup_records
        .iter()
        .find(|record| record.event_id == proposal_event_id)
        .expect("restore should rebuild the Allowance Event dedupe record");
    assert_eq!(dedupe.first_stream_item_id, original_stream_item_id);
    assert_eq!(
        dedupe.duplicate_stream_item_ids,
        vec![replay_stream_item_id]
    );
    assert!(dedupe.conflicting_stream_item_ids.is_empty());
    assert!(migrated.private_stream_items.iter().all(|item| {
        item.parse_status == PrivateStreamParseStatus::Valid
            && item.known_paykit_kind == item.parsed_kind
    }));
    let restored_record = allowance(&restored_bob, &pair.alice, &allowance_id).await;
    assert_eq!(restored_record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(
        restored_record.history_status,
        AllowanceHistoryStatus::Consistent
    );

    wait_until_marker_is_newer_than_observer_checkpoint(
        &pair.alice,
        &restored_bob.public_key,
        &restored_bob.receiver_path,
    )
    .await;
    restored_bob
        .sdk
        .publish_encrypted_link_recovery_marker(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
        )
        .await
        .expect("restored Bob should publish a recovery marker");
    assert_eq!(
        allowance(&restored_bob, &pair.alice, &allowance_id)
            .await
            .history_status,
        AllowanceHistoryStatus::RecoveryRequired
    );
    let observed = pair
        .alice
        .sdk
        .observe_encrypted_link_recovery_marker(
            restored_bob.public_key.clone(),
            restored_bob.receiver_path.clone(),
        )
        .await
        .expect("Alice should observe Bob's recovery marker");
    assert!(observed.remote_marker_changed);
    assert_eq!(
        allowance(&pair.alice, &restored_bob, &allowance_id)
            .await
            .history_status,
        AllowanceHistoryStatus::RecoveryRequired
    );

    let blocked = restored_bob
        .sdk
        .accept_allowance(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
            &allowance_id,
        )
        .await
        .expect_err("Allowance commands must fail closed during link recovery");
    assert!(matches!(blocked, PaykitSdkError::RecoveryRequired { .. }));

    drive_recovery_to_linked(&pair.alice, &restored_bob).await;
    for record in [
        allowance(&pair.alice, &restored_bob, &allowance_id).await,
        allowance(&restored_bob, &pair.alice, &allowance_id).await,
    ] {
        assert_eq!(record.state, AllowanceLifecycleState::Proposed);
        assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
    }

    restored_bob
        .sdk
        .accept_allowance(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
            &allowance_id,
        )
        .await
        .expect("restored Bob should continue the Allowance lifecycle");
    deliver(&restored_bob, &pair.alice).await;
    for record in [
        allowance(&pair.alice, &restored_bob, &allowance_id).await,
        allowance(&restored_bob, &pair.alice, &allowance_id).await,
    ] {
        assert_eq!(record.state, AllowanceLifecycleState::Accepted);
        assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
    }
}
