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
    let proposal_confirmation = storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .into_iter()
        .find(|message| message.is_delivery_confirmation())
        .unwrap();
    assert_eq!(
        proposal_confirmation.status,
        OutboundPrivateMessageStatus::Pending
    );
    let accepted = enqueue_allowance_response(
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
        &test_app_operation(&storage).await,
        FixedClock.now(),
    )
    .await
    .unwrap();
    assert_eq!(pending.undelivered_private_events, 1);
    let acceptance_message = storage
        .transaction(|tx| {
            let mut message = tx
                .outbound_private_messages(&counterparty)
                .into_iter()
                .find(|message| message.kind == PrivateMessageKind::AllowanceAcceptance.as_str())
                .unwrap();
            message.attempt_count = 1;
            message.last_attempt_at = Some(FixedClock.now());
            let message =
                crate::domain::outbound_private::mark_outbound_sent(message, FixedClock.now());
            tx.save_outbound_private_message(message.clone())?;
            Ok(message)
        })
        .await
        .unwrap();
    let published = crate::runtime::app_removal::begin_paykit_app_removal(
        &storage,
        &test_app_operation(&storage).await,
        FixedClock.now(),
    )
    .await
    .unwrap();
    assert_eq!(published.undelivered_private_events, 1);

    // Publication is not durable receipt. The peer must confirm the acceptance,
    // independently of our pending confirmation of its proposal.
    let confirmation = paykit_lib::DeliveryConfirmation::new(
        app_id(),
        EventId::new(accepted.acceptance_event_id.as_deref().unwrap()).unwrap(),
        crate::domain::private_stream::payload_hash(&acceptance_message.raw_json),
    )
    .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![PrivateApplicationMessage {
            version: Some(1),
            kind: Some(PrivateMessageKind::DeliveryConfirmation.as_str().into()),
            app_id: Some(app_id().as_str().into()),
            raw_json: paykit_lib::serialize_delivery_confirmation(&confirmation).unwrap(),
        }],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let confirmed_queue = storage.snapshot().unwrap().outbound_private_messages;
    let confirmed_acceptance = confirmed_queue
        .iter()
        .find(|message| message.outbound_message_id == acceptance_message.outbound_message_id)
        .unwrap();
    assert_eq!(confirmed_acceptance.confirmed_at, Some(FixedClock.now()));
    assert_eq!(
        confirmed_queue
            .iter()
            .filter(|message| message.is_delivery_confirmation())
            .collect::<Vec<_>>(),
        vec![&proposal_confirmation],
    );
    let delivered = crate::runtime::app_removal::begin_paykit_app_removal(
        &storage,
        &test_app_operation(&storage).await,
        FixedClock.now(),
    )
    .await
    .unwrap();
    assert!(delivered.is_empty());
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages,
        confirmed_queue
    );
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
    let after = storage.snapshot().unwrap().outbound_private_messages;
    assert_eq!(after.len(), confirmed_queue.len() + 1);
    assert_eq!(&after[..confirmed_queue.len()], confirmed_queue.as_slice());
    assert_eq!(
        after.last().unwrap().kind,
        PrivateMessageKind::AllowanceEnd.as_str(),
    );
    assert_eq!(after.last().unwrap().app_id.as_str(), "server");
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
    let before = storage.snapshot().unwrap().outbound_private_messages;

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
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages,
        before
    );
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
