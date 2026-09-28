use super::*;
use crate::test_utils::allowance_application_message;
use paykit_lib::{AllowanceEvent, AllowanceProposal, AllowanceRole, EventId};

#[tokio::test]
async fn test_allowance_mutations_reject_concurrent_identity_operation() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let allowance_id = AllowanceId::new_v4();
    let _guard = sdk.claim_identity_operation("test operation").unwrap();
    let results = [
        sdk.propose_allowance(
            counterparty.clone(),
            AllowanceLocalRole::Allower,
            AllowanceTerms::builder("btc")
                .lifetime_amount_limit("1")
                .build()
                .unwrap(),
        )
        .await,
        sdk.accept_allowance(counterparty.clone(), &allowance_id)
            .await,
        sdk.reject_allowance(counterparty.clone(), &allowance_id)
            .await,
        sdk.end_allowance(counterparty, &allowance_id).await,
    ];
    for result in results {
        assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    }
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}

#[tokio::test]
async fn test_app_removal_preserves_identity_wide_allowance_authority() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    storage
        .transaction(|tx| {
            let mut peer = default_linked_peer(counterparty.clone());
            peer.state = LinkedPeerState::Linked;
            tx.save_linked_peer(peer);
            Ok(())
        })
        .await
        .unwrap();
    let allowance_id = AllowanceId::new_v4();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![allowance_message(
            allowance_id.as_str(),
            EventId::new_v4().as_str(),
            AllowanceRole::Allower,
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    enqueue_allowance_response(
        &storage,
        counterparty.clone(),
        app_id(),
        allowance_id.clone(),
        AllowanceResponse::Acceptance,
        FixedClock.now(),
    )
    .await
    .unwrap();

    let pending = crate::runtime::app_removal::begin_paykit_app_removal(
        &storage,
        &app_id(),
        FixedClock.now(),
    )
    .await
    .unwrap();
    assert_eq!(pending.undelivered_private_events, 1);
    storage
        .transaction(|tx| {
            let mut message = tx.export_storage_state().outbound_private_messages[0].clone();
            message.status = OutboundPrivateMessageStatus::Sent;
            tx.save_outbound_private_message(message)
        })
        .await
        .unwrap();
    let delivered = crate::runtime::app_removal::begin_paykit_app_removal(
        &storage,
        &app_id(),
        FixedClock.now(),
    )
    .await
    .unwrap();
    assert!(delivered.is_empty());
    let record = derive_allowance_record(&storage, &counterparty, &allowance_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.state, crate::AllowanceLifecycleState::Accepted);

    let ended = enqueue_allowance_end(
        &storage,
        counterparty,
        paykit_lib::PaykitAppId::new("server").unwrap(),
        allowance_id,
        FixedClock.now(),
    )
    .await
    .unwrap();
    assert_eq!(ended.state, crate::AllowanceLifecycleState::Ended);
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages[1]
            .app_id
            .as_str(),
        "server"
    );
}

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
async fn test_list_and_get_allowances_preserve_counterparty_identity_scope() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    let wallet_allowance_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";
    let other_counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let server_allowance_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab45";
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
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
        other_counterparty.clone(),
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
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let listed = sdk
        .list_allowances(AllowanceFilter {
            counterparty: Some(counterparty.clone()),
            local_role: Some(AllowanceLocalRole::Allowee),
            ..AllowanceFilter::default()
        })
        .await
        .unwrap();
    let wrong_link = sdk
        .allowance_record(
            &other_counterparty,
            &AllowanceId::new(wallet_allowance_id).unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].allowance_id, wallet_allowance_id);
    assert_eq!(listed[0].counterparty, counterparty);
    assert!(wrong_link.is_none());
}

#[tokio::test]
async fn test_allowance_commands_require_session_without_queue_mutation() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    let allowance_id = AllowanceId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44").unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
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
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let proposed = sdk
        .propose_allowance(
            counterparty.clone(),
            AllowanceLocalRole::Allower,
            AllowanceTerms::builder("btc")
                .lifetime_amount_limit("1")
                .build()
                .unwrap(),
        )
        .await;
    let accepted = sdk
        .accept_allowance(counterparty.clone(), &allowance_id)
        .await;
    let rejected = sdk
        .reject_allowance(counterparty.clone(), &allowance_id)
        .await;
    let ended = sdk.end_allowance(counterparty, &allowance_id).await;

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
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut peer = default_linked_peer(counterparty);
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
        PaykitSdkConfig::new("bitkit").unwrap(),
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
