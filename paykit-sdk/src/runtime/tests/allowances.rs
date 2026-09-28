use super::*;
use crate::test_utils::allowance_application_message;
use paykit_lib::{AllowanceEvent, AllowanceProposal, AllowanceRole, EventId};

fn allowance_message(
    allowance_id: &str,
    event_id: &str,
    proposer_role: AllowanceRole,
) -> PrivateApplicationMessage {
    allowance_application_message(&AllowanceEvent::Proposal(AllowanceProposal::new(
        EventId::new(event_id).unwrap(),
        AllowanceId::new(allowance_id).unwrap(),
        proposer_role,
        AllowanceTerms::builder("btc")
            .lifetime_amount_limit("1")
            .build()
            .unwrap(),
    )))
}

#[tokio::test]
async fn test_list_and_get_allowances_preserve_exact_link_scope() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_initialized_identity_and_link(&storage, counterparty.clone()).await;
    let wallet_allowance_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";
    let server_allowance_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab45";
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![allowance_message(
            wallet_allowance_id,
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
            AllowanceRole::Allower,
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        other_receiver_path(),
        vec![allowance_message(
            server_allowance_id,
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202",
            AllowanceRole::Allowee,
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let listed = sdk
        .list_allowances(AllowanceFilter {
            counterparty: Some(counterparty.clone()),
            counterparty_receiver_path: Some(receiver_path()),
            local_role: Some(AllowanceLocalRole::Allowee),
            ..AllowanceFilter::default()
        })
        .await
        .unwrap();
    let wrong_link = sdk
        .allowance_record(
            &counterparty,
            &other_receiver_path(),
            &AllowanceId::new(wallet_allowance_id).unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].allowance_id, wallet_allowance_id);
    assert_eq!(listed[0].counterparty_receiver_path, receiver_path());
    assert!(wrong_link.is_none());
}

#[tokio::test]
async fn test_allowance_commands_require_session_without_queue_mutation() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_initialized_identity_and_link(&storage, counterparty.clone()).await;
    let allowance_id = AllowanceId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44").unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![allowance_message(
            allowance_id.as_str(),
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
            AllowanceRole::Allower,
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let proposed = sdk
        .propose_allowance(
            counterparty.clone(),
            receiver_path(),
            AllowanceLocalRole::Allower,
            AllowanceTerms::builder("btc")
                .lifetime_amount_limit("1")
                .build()
                .unwrap(),
        )
        .await;
    let accepted = sdk
        .accept_allowance(counterparty.clone(), receiver_path(), &allowance_id)
        .await;
    let rejected = sdk
        .reject_allowance(counterparty.clone(), receiver_path(), &allowance_id)
        .await;
    let ended = sdk
        .end_allowance(counterparty, receiver_path(), &allowance_id)
        .await;

    for result in [proposed, accepted, rejected, ended] {
        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}

#[tokio::test]
async fn test_list_allowances_blocked_peer_policy() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_initialized_identity_and_link(&storage, counterparty.clone()).await;
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut peer = default_linked_peer(counterparty, receiver_path());
                peer.state = LinkedPeerState::Blocked;
                tx.save_linked_peer(peer);
                Ok(())
            }
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![allowance_message(
            "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44",
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
            AllowanceRole::Allower,
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let targeted = sdk
        .list_allowances(AllowanceFilter {
            counterparty: Some(counterparty),
            ..AllowanceFilter::default()
        })
        .await;
    let unfiltered = sdk
        .list_allowances(AllowanceFilter::default())
        .await
        .unwrap();

    assert!(matches!(targeted, Err(PaykitSdkError::Policy { .. })));
    assert!(unfiltered.is_empty());
}
