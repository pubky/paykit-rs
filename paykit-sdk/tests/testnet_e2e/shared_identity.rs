use std::{
    any::Any,
    future::pending,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use paykit_lib::{
    BillingPeriod, PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms, Recurrence, RecurrenceUnit,
};
use paykit_sdk::{
    storage::StorageTransactionCallback, Clock, ContactUpdate, InMemoryStorage, LinkedPeerState,
    OutboundPrivateMessageStatus, PaykitApp, PaykitAppCapabilities, PaykitAppId, PaykitSdk,
    PaykitSdkConfig, PaykitSdkError, PaymentRequestLifecycleState,
    PrivatePaymentEndpointReservation, PrivateReceivingDetail, PubkyIdentityCapability,
    PubkyLocalSecretKey, PubkyPublicKey, PubkySessionAccess, PubkySessionBootstrap,
    PubkySharedStateStorage, ReceiptDraftBuilder, ReceiptIssuanceStatus, Result as PaykitResult,
    StorageAdapter, PAYKIT_SESSION_CAPABILITIES,
};
use pubky_testnet::EphemeralTestnet;
use serde_json::Map as JsonMap;
use tokio::sync::oneshot;

use crate::harness::{
    app_id, build_testnet, linked_two_party, private_receiving_detail, session_bootstrap, TestUser,
    TestnetPaymentAdapter, TestnetSessionProvider,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_independent_apps_register_concurrently_without_lost_updates() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let bitkit_result = session_bootstrap(&testnet, "bitkit.test")
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap();
    let server_result = session_bootstrap(&testnet, "paykit-server.test")
        .sign_in(&secret, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap();
    let bitkit_provider = TestnetSessionProvider::new(bitkit_result.access.clone());
    let server_provider = TestnetSessionProvider::new(server_result.access);
    let bitkit = PaykitSdk::new(
        InMemoryStorage::new(),
        bitkit_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    let server = PaykitSdk::new(
        InMemoryStorage::new(),
        server_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("paykit-server").unwrap(),
    );
    bitkit.initialize().await.unwrap();
    server.initialize().await.unwrap();

    let (bitkit_registry, server_registry) = tokio::join!(
        bitkit.publish_paykit_app(test_app("Bitkit")),
        server.publish_paykit_app(test_app("Paykit Server")),
    );
    bitkit_registry.unwrap();
    server_registry.unwrap();

    let registry = bitkit
        .paykit_app_registry(bitkit_result.public_key)
        .await
        .unwrap()
        .unwrap();
    assert!(registry.apps().contains_key(&app_id("bitkit")));
    assert!(registry.apps().contains_key(&app_id("paykit-server")));
}

#[tokio::test]
async fn test_pubky_shared_state_is_visible_to_independent_apps_and_survives_sign_out() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap(), "paykit-sdk.test")
        .unwrap()
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;

    let mut public_only_access = access.clone();
    public_only_access.local_secret_key = None;
    let public_only_storage =
        PubkySharedStateStorage::new(TestnetSessionProvider::new(public_only_access));
    let error = public_only_storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PaykitSdkError::Identity { context, .. }
            if context.contains("requires the Paykit identity secret")
    ));

    let bitkit_provider = TestnetSessionProvider::new(access.clone());
    let bitkit = PaykitSdk::new(
        PubkySharedStateStorage::new(bitkit_provider.clone()),
        bitkit_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    bitkit.initialize().await.unwrap();
    bitkit.publish_paykit_app(test_app("Bitkit")).await.unwrap();

    let server_provider = TestnetSessionProvider::new(access);
    let server_storage = PubkySharedStateStorage::new(server_provider.clone());
    let server = PaykitSdk::new(
        server_storage.clone(),
        server_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("paykit-server").unwrap(),
    );
    server.initialize().await.unwrap();
    server
        .publish_paykit_app(test_app("Paykit Server"))
        .await
        .unwrap();

    let before_sign_out = server_storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    assert_eq!(before_sign_out.registered_paykit_apps.len(), 2);

    bitkit.sign_out().await.unwrap();

    let after_sign_out = server_storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    assert_eq!(after_sign_out, before_sign_out);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_same_app_devices_serialize_public_endpoint_sync() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let first_result = session_bootstrap(&testnet, "bitkit-first.test")
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .expect("the first Bitkit device should sign up");
    let second_result = session_bootstrap(&testnet, "bitkit-second.test")
        .sign_in(&secret, PAYKIT_SESSION_CAPABILITIES)
        .await
        .expect("the second Bitkit device should receive an independent grant");
    let first = SharedStateTestUser::new(
        first_result.access,
        first_result.public_key.clone(),
        app_id("bitkit"),
        "Bitkit",
    )
    .await;
    let second = SharedStateTestUser::new(
        second_result.access,
        second_result.public_key,
        app_id("bitkit"),
        "Bitkit",
    )
    .await;
    first
        .adapter
        .set_public_details(vec![crate::harness::public_receiving_detail(
            "btc-lightning-bolt11",
            "first-device",
        )]);
    second
        .adapter
        .set_public_details(vec![crate::harness::public_receiving_detail(
            "btc-lightning-bolt11",
            "second-device",
        )]);

    let (loaded, resume) = first.adapter.pause_next_public_details_load();
    let first_sync = first.sdk.sync_public_endpoints();
    let second_sync = async {
        loaded
            .await
            .expect("the first sync should hold the shared App lease");
        let result = second.sdk.sync_public_endpoints().await;
        let _ = resume.send(());
        result
    };
    let (first_result, second_result) = tokio::join!(first_sync, second_sync);

    first_result.expect("the lease holder should finish endpoint sync");
    assert!(matches!(second_result, Err(PaykitSdkError::Policy { .. })));

    second
        .sdk
        .sync_public_endpoints()
        .await
        .expect("the second device should retry after lease release");
    let list = paykit_lib::get_payment_list(
        &second.access.outbox_client.public_storage(),
        second.access.session.info().public_key(),
        &second.app_id,
    )
    .await
    .expect("the final endpoint list should remain readable");
    assert_eq!(
        list.payment_endpoints
            .get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap())
            .map(|payload| payload.as_str()),
        Some("second-device")
    );

    first.adapter.set_public_details(Vec::new());
    second
        .adapter
        .set_public_details(vec![crate::harness::public_receiving_detail(
            "btc-lightning-bolt11",
            "third-device",
        )]);
    let (loaded, resume) = first.adapter.pause_next_public_details_load();
    let removal = first.sdk.sync_public_endpoints();
    let blocked_publication = async {
        loaded
            .await
            .expect("the removal should hold the shared App lease");
        let result = second.sdk.sync_public_endpoints().await;
        let _ = resume.send(());
        result
    };
    let (removal, blocked_publication) = tokio::join!(removal, blocked_publication);
    removal.expect("the lease holder should remove the endpoint");
    assert!(matches!(
        blocked_publication,
        Err(PaykitSdkError::Policy { .. })
    ));
    let empty = paykit_lib::get_payment_list(
        &first.access.outbox_client.public_storage(),
        first.access.session.info().public_key(),
        &first.app_id,
    )
    .await
    .expect("the removal should leave an empty endpoint list");
    assert!(empty.payment_endpoints.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_shared_apps_advance_one_handshake_without_diverging() {
    let pair = homeserver_shared_pair().await;
    pair.bitkit
        .sdk
        .initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .expect("the shared identity should initiate the handshake");
    pair.bob
        .sdk
        .accept_link_with_peer(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should accept the handshake");

    let (bitkit_advance, server_advance) = tokio::join!(
        pair.bitkit
            .sdk
            .advance_link_handshake(pair.bob.public_key.clone()),
        pair.server
            .sdk
            .advance_link_handshake(pair.bob.public_key.clone()),
    );
    assert!(bitkit_advance.is_ok() || server_advance.is_ok());
    for result in [bitkit_advance, server_advance] {
        assert!(
            result.is_ok() || matches!(result, Err(PaykitSdkError::Policy { .. })),
            "concurrent handshake advancement should either advance or observe the peer lease"
        );
    }

    drive_shared_link_to_linked(&pair.bitkit, &pair.bob).await;
    let state = pair.server.storage_state().await;
    assert_eq!(
        state
            .linked_peers
            .get(&pair.bob.public_key)
            .expect("the shared peer should remain present")
            .state,
        LinkedPeerState::Linked
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_shared_apps_observe_one_recovery_marker_without_diverging() {
    let pair = linked_homeserver_shared_pair().await;
    pair.bitkit
        .sdk
        .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .expect("the shared identity should publish its recovery marker");
    pair.bob
        .sdk
        .observe_encrypted_link_recovery_marker(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should observe the shared identity's marker");
    pair.bob
        .sdk
        .publish_encrypted_link_recovery_marker(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should publish a recovery marker");

    let (bitkit_observe, server_observe) = tokio::join!(
        pair.bitkit
            .sdk
            .observe_encrypted_link_recovery_marker(pair.bob.public_key.clone()),
        pair.server
            .sdk
            .observe_encrypted_link_recovery_marker(pair.bob.public_key.clone()),
    );
    assert!(bitkit_observe.is_ok() || server_observe.is_ok());
    for result in [&bitkit_observe, &server_observe] {
        assert!(
            result.is_ok() || matches!(result, Err(PaykitSdkError::Policy { .. })),
            "concurrent recovery should either observe the marker or the peer lease"
        );
    }
    let state = pair.bitkit.storage_state().await;
    assert_eq!(
        state
            .linked_peers
            .get(&pair.bob.public_key)
            .expect("the recovered peer should remain present")
            .state,
        LinkedPeerState::RecoveryRequired,
        "concurrent recovery reports: bitkit={bitkit_observe:?}, server={server_observe:?}"
    );

    pair.bitkit
        .sdk
        .ensure_link_with_peer(pair.bob.public_key.clone(), 0)
        .await
        .expect("the shared identity should start a fresh handshake");
    pair.bob
        .sdk
        .ensure_link_with_peer(pair.bitkit.public_key.clone(), 0)
        .await
        .expect("the peer should start its side of the fresh handshake");
    drive_shared_link_to_linked(&pair.bitkit, &pair.bob).await;
}

#[tokio::test]
async fn test_paykit_identity_key_rotation_rekeys_shared_state_and_registry() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap(), "paykit-sdk.test")
        .unwrap()
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    let provider = TestnetSessionProvider::new(access.clone());
    let storage = PubkySharedStateStorage::new(provider.clone());
    let sdk = PaykitSdk::new(
        storage.clone(),
        provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    let initialized = sdk.initialize().await.unwrap();
    let owner = initialized
        .public_key
        .clone()
        .expect("initialized SDK should report its identity");
    sdk.publish_paykit_app(test_app("Bitkit")).await.unwrap();

    let replacement = secret
        .derive_paykit_identity_secret_key(2)
        .expect("replacement Paykit key derivation should succeed");
    let registry = sdk
        .rotate_paykit_identity_key(replacement.clone())
        .await
        .expect("Paykit key rotation should succeed");
    assert_eq!(registry.key_generation(), 2);

    let old_key_error = storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .expect_err("the previous Paykit key must not decrypt rotated state");
    assert!(matches!(old_key_error, PaykitSdkError::Identity { .. }));

    let mut replacement_access = access;
    replacement_access.paykit_identity_secret_key = Some(replacement);
    let replacement_provider = TestnetSessionProvider::new(replacement_access);
    let replacement_storage = PubkySharedStateStorage::new(replacement_provider.clone());
    let replacement_sdk = PaykitSdk::new(
        replacement_storage.clone(),
        replacement_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    let resumed = replacement_sdk.initialize().await.unwrap();
    assert_eq!(resumed.public_key, initialized.public_key);

    let state = replacement_storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    assert_eq!(state.registered_paykit_apps.len(), 1);
    let registry = replacement_sdk
        .paykit_app_registry(owner)
        .await
        .unwrap()
        .expect("rotated App Registry should remain published");
    assert_eq!(registry.key_generation(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pubky_shared_state_rejects_a_stale_writer() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap(), "paykit-sdk.test")
        .unwrap()
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    let first_provider = TestnetSessionProvider::new(access.clone());
    let first = PubkySharedStateStorage::new(first_provider.clone());
    let second = PubkySharedStateStorage::new(TestnetSessionProvider::new(access));
    let sdk = PaykitSdk::new(
        first.clone(),
        first_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    sdk.initialize().await.unwrap();
    let initialized_at = first
        .transaction(|tx| Ok(tx.load_identity_state().unwrap().initialized_at))
        .await
        .unwrap();

    let (loaded_tx, loaded_rx) = std::sync::mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(0);
    let stale = first.clone();
    let stale_write = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(stale.transaction(move |tx| {
                loaded_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                let mut identity = tx.load_identity_state().unwrap();
                identity.initialized_at += chrono::Duration::seconds(2);
                tx.save_identity_state(identity);
                Ok(())
            }))
    });
    loaded_rx.recv().unwrap();
    second
        .transaction(|tx| {
            let mut identity = tx.load_identity_state().unwrap();
            identity.initialized_at += chrono::Duration::seconds(1);
            tx.save_identity_state(identity);
            Ok(())
        })
        .await
        .unwrap();
    continue_tx.send(()).unwrap();

    let error = stale_write.join().unwrap().unwrap_err();
    assert!(matches!(
        error,
        PaykitSdkError::ConcurrentUpdate { context, .. }
            if context.contains("changed during transaction")
    ));

    first
        .transaction(|tx| {
            let mut identity = tx.load_identity_state().unwrap();
            identity.initialized_at += chrono::Duration::seconds(3);
            tx.save_identity_state(identity);
            Ok(())
        })
        .await
        .expect("the stale storage instance should succeed after reloading and retrying");
    let final_initialized_at = second
        .transaction(|tx| Ok(tx.load_identity_state().unwrap().initialized_at))
        .await
        .unwrap();
    assert_eq!(
        final_initialized_at,
        initialized_at + chrono::Duration::seconds(4)
    );
}

#[tokio::test]
async fn test_pubky_shared_state_rejects_a_missing_previously_observed_resource() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap(), "paykit-sdk.test")
        .unwrap()
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    let provider = TestnetSessionProvider::new(access.clone());
    let storage = PubkySharedStateStorage::new(provider.clone());
    let sdk = PaykitSdk::new(
        storage.clone(),
        provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    sdk.initialize().await.unwrap();
    let observer = PubkySharedStateStorage::new(TestnetSessionProvider::new(access.clone()));
    let callback_error = observer
        .transaction(|_| -> paykit_sdk::Result<()> {
            Err(PaykitSdkError::Policy {
                context: "test callback failure".into(),
                source: None,
            })
        })
        .await
        .unwrap_err();
    assert!(matches!(callback_error, PaykitSdkError::Policy { .. }));
    access
        .session
        .storage()
        .delete(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
        .unwrap();

    let error = observer
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        PaykitSdkError::Storage { context, .. }
            if context.contains("previously observed Pubky shared state is missing")
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_independent_grants_share_homeserver_noise_state_under_concurrency() {
    let pair = linked_homeserver_shared_pair().await;

    let first_outbound = pair
        .bitkit
        .sdk
        .propose_payment_request(pair.bob.public_key.clone(), recurring_request_terms())
        .await
        .expect("Bitkit should queue its Payment Request");
    let second_outbound = pair
        .server
        .sdk
        .propose_payment_request(pair.bob.public_key.clone(), recurring_request_terms())
        .await
        .expect("Paykit Server should queue its Payment Request");

    let (bitkit_send_sdk, send_loaded, continue_send) = pair.bitkit.paused_sdk();
    let send_counterparty = pair.bob.public_key.clone();
    let stale_send_sdk = bitkit_send_sdk.clone();
    let stale_send = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(stale_send_sdk.process_outbound_private_messages(send_counterparty))
    });
    send_loaded
        .recv()
        .expect("the stale sender should load shared state");
    let successful_send = pair
        .server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the concurrent sender should commit both queued messages");
    assert_eq!(successful_send.sent.len(), 2);
    continue_send
        .send(())
        .expect("the stale sender should resume");
    let retried_send = stale_send
        .join()
        .unwrap()
        .expect("the stale sender should reload shared state and retry internally");
    assert!(retried_send.attempted.is_empty());

    let received_outbound = pair
        .bob
        .sdk
        .receive_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should decrypt both shared-state sends");
    assert_eq!(received_outbound.stream_item_ids.len(), 2);
    let received_ids = pair
        .bob
        .sdk
        .received_payment_requests_from(&pair.bitkit.public_key)
        .await
        .expect("the peer should derive both Payment Requests")
        .into_iter()
        .map(|request| request.payment_request_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(received_ids.len(), 2);
    assert!(received_ids.contains(&first_outbound.payment_request_id));
    assert!(received_ids.contains(&second_outbound.payment_request_id));

    pair.bob
        .sdk
        .propose_payment_request(pair.bitkit.public_key.clone(), recurring_request_terms())
        .await
        .expect("the peer should queue the first inbound Payment Request");
    pair.bob
        .sdk
        .propose_payment_request(pair.bitkit.public_key.clone(), recurring_request_terms())
        .await
        .expect("the peer should queue the second inbound Payment Request");
    let peer_send = pair
        .bob
        .sdk
        .process_outbound_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should send both inbound Payment Requests");
    assert_eq!(peer_send.sent.len(), 2);

    let (bitkit_receive_sdk, receive_loaded, continue_receive) = pair.bitkit.paused_sdk();
    let receive_counterparty = pair.bob.public_key.clone();
    let stale_receive_sdk = bitkit_receive_sdk.clone();
    let stale_receive = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(stale_receive_sdk.receive_private_messages(receive_counterparty))
    });
    receive_loaded
        .recv()
        .expect("the stale receiver should load shared state");
    let successful_receive = pair
        .server
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the concurrent receiver should commit both inbound messages");
    assert_eq!(successful_receive.stream_item_ids.len(), 2);
    continue_receive
        .send(())
        .expect("the stale receiver should resume");
    let retried_receive = stale_receive
        .join()
        .unwrap()
        .expect("the stale receiver should reload shared state and retry internally");
    assert!(retried_receive.stream_item_ids.is_empty());

    let bitkit_state = pair.bitkit.storage_state().await;
    let server_state = pair.server.storage_state().await;
    assert_eq!(bitkit_state, server_state);
    assert!(bitkit_state.peer_link_operation_leases.is_empty());
    assert!(bitkit_state
        .outbound_private_messages
        .iter()
        .all(
            |message| message.status == OutboundPrivateMessageStatus::Sent
                && message.prepared_send.is_none()
        ));
    assert_eq!(
        bitkit_state
            .private_stream_items
            .iter()
            .filter(|item| item.counterparty == pair.bob.public_key)
            .count(),
        2
    );
}

#[tokio::test]
async fn test_prepared_private_sends_survive_restart_at_both_crash_boundaries() {
    let pair = linked_homeserver_shared_pair().await;

    assert_private_send_survives_restart(&pair, PrivateOperationCrashPoint::PreparedStateCommitted)
        .await;
    assert_private_send_survives_restart(&pair, PrivateOperationCrashPoint::CiphertextPublished)
        .await;
}

#[tokio::test]
async fn test_private_receive_survives_restart_after_checkpoint_commit() {
    let pair = linked_homeserver_shared_pair().await;
    pair.bob
        .sdk
        .propose_payment_request(pair.bitkit.public_key.clone(), recurring_request_terms())
        .await
        .expect("the peer should queue a Payment Request");
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should send the Payment Request");

    let crash_time = Utc::now();
    let (sdk, reached) = pair.bitkit.crashable_sdk(
        PrivateOperationCrashPoint::PrivateReceiveCheckpointCommitted,
        crash_time,
    );
    let counterparty = pair.bob.public_key.clone();
    let receiving = tokio::spawn(async move { sdk.receive_private_messages(counterparty).await });
    reached
        .await
        .expect("the receive should reach the committed-checkpoint crash boundary");
    receiving.abort();
    assert!(receiving
        .await
        .expect_err("the receive task should be aborted")
        .is_cancelled());

    let restarted = pair
        .bitkit
        .restarted_sdk(crash_time + chrono::Duration::seconds(61));
    let replay = restarted
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the restarted receiver should resume from the committed checkpoint");
    assert!(replay.stream_item_ids.is_empty());
    let state = pair.bitkit.storage_state().await;
    assert_eq!(
        state
            .private_stream_items
            .iter()
            .filter(|item| item.counterparty == pair.bob.public_key)
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_key_rotation_rejects_an_in_flight_old_key_writer() {
    let pair = linked_homeserver_shared_pair().await;
    let (old_sdk, loaded, resume) = pair.server.paused_sdk();
    let contact = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let stale_write = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(old_sdk.save_contact(ContactUpdate {
                public_key: contact,
                label: Some("stale writer".into()),
            }))
    });
    loaded
        .recv()
        .expect("the old-key writer should load shared state");

    let replacement = pair
        .secret
        .derive_paykit_identity_secret_key(2)
        .expect("the replacement key should derive");
    pair.bitkit
        .sdk
        .rotate_paykit_identity_key(replacement.clone())
        .await
        .expect("key rotation should commit");
    resume
        .send(())
        .expect("the old-key writer should resume after rotation");
    let stale_error = stale_write
        .join()
        .unwrap()
        .expect_err("the old-key writer must not overwrite rotated state");
    assert!(stale_error.is_concurrent_update());

    let mut replacement_access = pair.bitkit.access.clone();
    replacement_access.paykit_identity_secret_key = Some(replacement);
    let replacement_provider = TestnetSessionProvider::new(replacement_access);
    let replacement_storage = PubkySharedStateStorage::new(replacement_provider.clone());
    let replacement_sdk = PaykitSdk::new(
        replacement_storage,
        replacement_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new(pair.bitkit.app_id.clone()).unwrap(),
    );
    replacement_sdk.initialize().await.unwrap();
    assert!(replacement_sdk.contact_records().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_homeserver_backed_apps_complete_private_payment_flow() {
    let pair = linked_homeserver_shared_pair().await;
    pair.bitkit
        .adapter
        .set_private_details(vec![private_receiving_detail(
            "btc-lightning-bolt11",
            "ln-bitkit-private",
        )]);
    pair.server
        .adapter
        .set_private_details(vec![private_receiving_detail(
            "btc-lightning-bolt11",
            "ln-server-private",
        )]);

    let bitkit_list = pair
        .bitkit
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .expect("Bitkit should queue its Private Payment List");
    let server_list = pair
        .server
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .expect("Paykit Server should queue its Private Payment List");
    let list_send = pair
        .server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("either shared application should deliver both private lists");
    assert_eq!(
        list_send.sent,
        vec![
            bitkit_list.outbound_message_id,
            server_list.outbound_message_id
        ]
    );
    pair.bob
        .sdk
        .receive_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the payer should receive both private lists");
    let private_lists = pair
        .bob
        .sdk
        .current_private_payment_lists(&pair.bitkit.public_key)
        .await
        .expect("the payer should read the aggregated private lists");
    assert_eq!(private_lists.len(), 2);
    assert!(private_lists
        .iter()
        .any(|list| list.app_id == pair.bitkit.app_id));
    assert!(private_lists
        .iter()
        .any(|list| list.app_id == pair.server.app_id));

    let mut terms = recurring_request_terms();
    terms.required_app_id = Some(pair.bitkit.app_id.clone());
    let payment_reference = terms.payment_reference.clone();
    let amount = terms.amount.clone();
    let request = pair
        .server
        .sdk
        .propose_payment_request(pair.bob.public_key.clone(), terms)
        .await
        .expect("Paykit Server should queue a Payment Request for Bitkit's endpoint");
    pair.server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("Paykit Server should deliver the Payment Request");
    pair.bob
        .sdk
        .receive_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the payer should receive the Payment Request");

    let payment_request_id = request_id(&request);
    pair.bob
        .sdk
        .claim_payment_request_for_execution(pair.bitkit.public_key.clone(), &payment_request_id)
        .await
        .expect("the payer should claim execution before accepting");
    pair.bob
        .sdk
        .accept_payment_request(pair.bitkit.public_key.clone(), &payment_request_id)
        .await
        .expect("the payer should accept the Payment Request");
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the payer should deliver the acceptance");
    pair.bitkit
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the shared identity should receive the acceptance");

    let billing_period = BillingPeriod {
        starts_at: "2026-08-01T00:00:00Z".into(),
        ends_at: "2026-09-01T00:00:00Z".into(),
    };
    pair.bob
        .sdk
        .submit_payment_proof(
            pair.bitkit.public_key.clone(),
            &payment_request_id,
            Some(billing_period.clone()),
            pair.bob.app_id.clone(),
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            JsonMap::from_iter([(
                "preimage".into(),
                serde_json::Value::String("test-preimage".into()),
            )]),
        )
        .await
        .expect("the payer should queue proof for the selected Bitkit endpoint");
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the payer should deliver the Payment Proof");
    pair.server
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("Paykit Server should receive the shared Payment Proof");
    let proven = request_with_id(
        pair.bitkit
            .sdk
            .payment_requests_with(&pair.bob.public_key)
            .await
            .expect("Bitkit should read the shared proven request"),
        payment_request_id.as_str(),
    );
    assert_eq!(proven.payment_proofs.len(), 1);

    let receipt = ReceiptDraftBuilder::from_payment_reference(payment_reference)
        .with_new_receipt_id()
        .with_payment_request_id(payment_request_id.clone())
        .with_billing_period(billing_period)
        .with_payment_endpoint_identifier(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        )
        .with_amount(amount)
        .build()
        .unwrap();
    let issuance = pair
        .server
        .sdk
        .issue_receipt(pair.bob.public_key.clone(), receipt)
        .await
        .expect("Paykit Server should store the Receipt and queue Receipt Access");
    assert_eq!(issuance.status, ReceiptIssuanceStatus::AccessQueued);
    pair.server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("Paykit Server should deliver Receipt Access");
    pair.bob
        .sdk
        .receive_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the payer should receive Receipt Access");
    let retrieved = pair
        .bob
        .sdk
        .retrieve_receipt(pair.bitkit.public_key.clone(), &issuance.receipt_id)
        .await
        .expect("the payer should fetch and decrypt the Receipt");
    assert_eq!(
        retrieved.payment_request_id.as_deref(),
        Some(payment_request_id.as_str())
    );
    assert_eq!(retrieved.app_id, pair.server.app_id);
}

type SharedStateSdk =
    PaykitSdk<PubkySharedStateStorage, TestnetSessionProvider, TestnetPaymentAdapter>;
type PausedSharedStateSdk =
    PaykitSdk<OneShotPausedStorage, TestnetSessionProvider, TestnetPaymentAdapter>;
type CrashableSharedStateSdk =
    PaykitSdk<OneShotCrashStorage, TestnetSessionProvider, TestnetPaymentAdapter, FixedTestClock>;
type RestartedSharedStateSdk = PaykitSdk<
    PubkySharedStateStorage,
    TestnetSessionProvider,
    TestnetPaymentAdapter,
    FixedTestClock,
>;

struct SharedStateTestUser {
    sdk: SharedStateSdk,
    storage: PubkySharedStateStorage,
    adapter: TestnetPaymentAdapter,
    access: PubkySessionAccess,
    public_key: PubkyPublicKey,
    app_id: PaykitAppId,
}

impl SharedStateTestUser {
    async fn new(
        access: PubkySessionAccess,
        public_key: PubkyPublicKey,
        app_id: PaykitAppId,
        display_name: &str,
    ) -> Self {
        let provider = TestnetSessionProvider::new(access.clone());
        let storage = PubkySharedStateStorage::new(provider.clone());
        let adapter = TestnetPaymentAdapter::default();
        let sdk = PaykitSdk::new(
            storage.clone(),
            provider,
            adapter.clone(),
            PaykitSdkConfig::new(app_id.clone()).unwrap(),
        );
        sdk.initialize()
            .await
            .expect("shared-state SDK initialization should succeed");
        sdk.publish_paykit_app(test_app(display_name))
            .await
            .expect("shared-state Paykit app publication should succeed");
        Self {
            sdk,
            storage,
            adapter,
            access,
            public_key,
            app_id,
        }
    }

    fn paused_sdk(&self) -> (Arc<PausedSharedStateSdk>, Receiver<()>, SyncSender<()>) {
        let provider = TestnetSessionProvider::new(self.access.clone());
        let (storage, loaded, resume) =
            OneShotPausedStorage::new(PubkySharedStateStorage::new(provider.clone()));
        let sdk = PaykitSdk::new(
            storage,
            provider,
            TestnetPaymentAdapter::default(),
            PaykitSdkConfig::new(self.app_id.clone()).unwrap(),
        );
        (Arc::new(sdk), loaded, resume)
    }

    fn crashable_sdk(
        &self,
        crash_point: PrivateOperationCrashPoint,
        now: chrono::DateTime<Utc>,
    ) -> (Arc<CrashableSharedStateSdk>, oneshot::Receiver<()>) {
        let provider = TestnetSessionProvider::new(self.access.clone());
        let (storage, reached) =
            OneShotCrashStorage::new(PubkySharedStateStorage::new(provider.clone()), crash_point);
        let sdk = PaykitSdk::with_clock(
            storage,
            provider,
            TestnetPaymentAdapter::default(),
            PaykitSdkConfig::new(self.app_id.clone()).unwrap(),
            FixedTestClock(now),
        );
        (Arc::new(sdk), reached)
    }

    fn restarted_sdk(&self, now: chrono::DateTime<Utc>) -> RestartedSharedStateSdk {
        let provider = TestnetSessionProvider::new(self.access.clone());
        PaykitSdk::with_clock(
            PubkySharedStateStorage::new(provider.clone()),
            provider,
            TestnetPaymentAdapter::default(),
            PaykitSdkConfig::new(self.app_id.clone()).unwrap(),
            FixedTestClock(now),
        )
    }

    async fn storage_state(&self) -> paykit_sdk::storage::StorageState {
        self.storage
            .transaction(|tx| Ok(tx.export_storage_state()))
            .await
            .expect("shared state should remain readable")
    }
}

struct HomeserverSharedPair {
    _testnet: EphemeralTestnet,
    secret: PubkyLocalSecretKey,
    bitkit: SharedStateTestUser,
    server: SharedStateTestUser,
    bob: TestUser,
}

async fn linked_homeserver_shared_pair() -> HomeserverSharedPair {
    let pair = homeserver_shared_pair().await;
    pair.bitkit
        .sdk
        .initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .expect("shared identity should initiate the Encrypted Link Handshake");
    pair.bob
        .sdk
        .accept_link_with_peer(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should accept the Encrypted Link Handshake");
    drive_shared_link_to_linked(&pair.bitkit, &pair.bob).await;
    pair
}

async fn homeserver_shared_pair() -> HomeserverSharedPair {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let bitkit_result = session_bootstrap(&testnet, "bitkit.test")
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .expect("shared identity sign-up should succeed");
    let server_result = session_bootstrap(&testnet, "paykit-server.test")
        .sign_in(&secret, PAYKIT_SESSION_CAPABILITIES)
        .await
        .expect("the second application should receive its own scoped grant");
    assert_eq!(bitkit_result.public_key, server_result.public_key);
    assert_ne!(
        bitkit_result
            .export_session_secret()
            .await
            .expect("Bitkit grant should be exportable")
            .as_str(),
        server_result
            .export_session_secret()
            .await
            .expect("Paykit Server grant should be exportable")
            .as_str(),
        "independent applications must not share one persisted grant"
    );
    let bitkit = SharedStateTestUser::new(
        bitkit_result.access,
        bitkit_result.public_key.clone(),
        app_id("bitkit"),
        "Bitkit",
    )
    .await;
    let server = SharedStateTestUser::new(
        server_result.access,
        server_result.public_key,
        app_id("paykit-server"),
        "Paykit Server",
    )
    .await;
    let bob = TestUser::sign_up(&testnet).await;

    HomeserverSharedPair {
        _testnet: testnet,
        secret,
        bitkit,
        server,
        bob,
    }
}

async fn assert_private_send_survives_restart(
    pair: &HomeserverSharedPair,
    crash_point: PrivateOperationCrashPoint,
) {
    let request = pair
        .bitkit
        .sdk
        .propose_payment_request(pair.bob.public_key.clone(), recurring_request_terms())
        .await
        .expect("the crashing application should queue a Payment Request");
    let outbound_message_id = request
        .proposal_outbound_message_id
        .expect("the local proposal should identify its outbound record");
    let crash_time = Utc::now();
    let (sdk, reached) = pair.bitkit.crashable_sdk(crash_point, crash_time);
    let counterparty = pair.bob.public_key.clone();
    let send =
        tokio::spawn(async move { sdk.process_outbound_private_messages(counterparty).await });

    tokio::time::timeout(Duration::from_secs(10), reached)
        .await
        .expect("the send should reach its deterministic crash boundary")
        .expect("the crash boundary sender should remain alive");

    let crashed_state = pair.bitkit.storage_state().await;
    let crashed_message = crashed_state
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == outbound_message_id)
        .expect("the prepared outbound record should remain durable");
    assert_eq!(
        crashed_message.status,
        OutboundPrivateMessageStatus::Sending
    );
    assert!(crashed_message.prepared_send.is_some());
    assert!(crashed_state
        .peer_link_operation_leases
        .contains_key(&pair.bob.public_key));
    let crashed_link = crashed_state
        .encrypted_link_states
        .get(&pair.bob.public_key)
        .expect("the advanced Encrypted Link snapshot should be durable")
        .clone();

    let received_before_restart = pair
        .bob
        .sdk
        .receive_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should inspect the private stream at the crash boundary");
    match crash_point {
        PrivateOperationCrashPoint::PreparedStateCommitted => {
            assert!(received_before_restart.stream_item_ids.is_empty());
        }
        PrivateOperationCrashPoint::CiphertextPublished => {
            assert_eq!(received_before_restart.stream_item_ids.len(), 1);
        }
        PrivateOperationCrashPoint::PrivateReceiveCheckpointCommitted => {
            unreachable!("receive crash point is not used by send recovery")
        }
    }

    send.abort();
    assert!(send
        .await
        .expect_err("the crashed sender should be aborted")
        .is_cancelled());

    let restarted = pair
        .bitkit
        .restarted_sdk(crash_time + chrono::Duration::seconds(61));
    let report = restarted
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the restarted application should reclaim and finish the prepared send");
    assert_eq!(report.attempted, vec![outbound_message_id]);
    assert_eq!(report.sent, vec![outbound_message_id]);

    let received_after_restart = pair
        .bob
        .sdk
        .receive_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should resume after the sender restart");
    match crash_point {
        PrivateOperationCrashPoint::PreparedStateCommitted => {
            assert_eq!(received_after_restart.stream_item_ids.len(), 1);
        }
        PrivateOperationCrashPoint::CiphertextPublished => {
            assert!(received_after_restart.stream_item_ids.is_empty());
        }
        PrivateOperationCrashPoint::PrivateReceiveCheckpointCommitted => {
            unreachable!("receive crash point is not used by send recovery")
        }
    }

    let received_request_count = pair
        .bob
        .sdk
        .received_payment_requests_from(&pair.bitkit.public_key)
        .await
        .expect("the peer should retain derived Payment Requests")
        .into_iter()
        .filter(|record| record.payment_request_id == request.payment_request_id)
        .count();
    assert_eq!(received_request_count, 1);

    let final_state = pair.bitkit.storage_state().await;
    let final_message = final_state
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == outbound_message_id)
        .expect("the completed outbound record should remain durable");
    assert_eq!(final_message.status, OutboundPrivateMessageStatus::Sent);
    assert_eq!(final_message.attempt_count, 2);
    assert!(final_message.prepared_send.is_none());
    assert!(final_state.peer_link_operation_leases.is_empty());
    assert_eq!(
        final_state
            .encrypted_link_states
            .get(&pair.bob.public_key)
            .expect("the Encrypted Link state should remain present"),
        &crashed_link,
        "retrying a prepared send must not advance Noise state again"
    );
}

async fn drive_shared_link_to_linked(alice: &SharedStateTestUser, bob: &TestUser) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut alice_state = LinkedPeerState::Linking;
    let mut bob_state = LinkedPeerState::Linking;
    while alice_state != LinkedPeerState::Linked || bob_state != LinkedPeerState::Linked {
        assert!(
            Instant::now() < deadline,
            "shared-state Encrypted Link Handshake timed out"
        );
        if alice_state != LinkedPeerState::Linked {
            alice_state = alice
                .sdk
                .advance_link_handshake(bob.public_key.clone())
                .await
                .expect("shared-state initiator handshake advance should succeed")
                .state;
        }
        if bob_state != LinkedPeerState::Linked {
            bob_state = bob
                .sdk
                .advance_link_handshake(alice.public_key.clone())
                .await
                .expect("peer handshake advance should succeed")
                .state;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(Clone)]
struct OneShotPausedStorage {
    inner: PubkySharedStateStorage,
    pause: Arc<Mutex<Option<TransactionPause>>>,
}

struct TransactionPause {
    loaded: SyncSender<()>,
    resume: Receiver<()>,
}

impl OneShotPausedStorage {
    fn new(inner: PubkySharedStateStorage) -> (Self, Receiver<()>, SyncSender<()>) {
        let (loaded_tx, loaded_rx) = sync_channel(0);
        let (resume_tx, resume_rx) = sync_channel(0);
        (
            Self {
                inner,
                pause: Arc::new(Mutex::new(Some(TransactionPause {
                    loaded: loaded_tx,
                    resume: resume_rx,
                }))),
            },
            loaded_rx,
            resume_tx,
        )
    }
}

#[async_trait]
impl StorageAdapter for OneShotPausedStorage {
    async fn transaction_erased<'a>(
        &self,
        transaction: StorageTransactionCallback<'a>,
    ) -> PaykitResult<Box<dyn Any + Send>> {
        let pause = self.pause.clone();
        self.inner
            .transaction_erased(Box::new(move |tx| {
                let before = tx.export_storage_state();
                let result = transaction(tx);
                if result.is_ok() && tx.export_storage_state() != before {
                    let pause = pause.lock().expect("pause lock poisoned").take();
                    if let Some(pause) = pause {
                        pause.loaded.send(()).expect("test should await the load");
                        pause.resume.recv().expect("test should resume the write");
                    }
                }
                result
            }))
            .await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateOperationCrashPoint {
    PreparedStateCommitted,
    CiphertextPublished,
    PrivateReceiveCheckpointCommitted,
}

#[derive(Clone, Copy)]
struct FixedTestClock(chrono::DateTime<Utc>);

impl Clock for FixedTestClock {
    fn now(&self) -> chrono::DateTime<Utc> {
        self.0
    }
}

#[derive(Clone)]
struct OneShotCrashStorage {
    inner: PubkySharedStateStorage,
    crash_point: PrivateOperationCrashPoint,
    prepared_committed: Arc<AtomicBool>,
    reached: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl OneShotCrashStorage {
    fn new(
        inner: PubkySharedStateStorage,
        crash_point: PrivateOperationCrashPoint,
    ) -> (Self, oneshot::Receiver<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        (
            Self {
                inner,
                crash_point,
                prepared_committed: Arc::new(AtomicBool::new(false)),
                reached: Arc::new(Mutex::new(Some(reached_tx))),
            },
            reached_rx,
        )
    }

    async fn stop_at_crash_boundary(&self) {
        let reached = self
            .reached
            .lock()
            .expect("crash signal lock poisoned")
            .take();
        if let Some(reached) = reached {
            reached
                .send(())
                .expect("the test should await the crash boundary");
            // The test aborts the blocked task so normal lease cleanup cannot run.
            pending::<()>().await;
        }
    }
}

#[async_trait]
impl StorageAdapter for OneShotCrashStorage {
    async fn transaction_erased<'a>(
        &self,
        transaction: StorageTransactionCallback<'a>,
    ) -> PaykitResult<Box<dyn Any + Send>> {
        // Publication is the only operation between prepared-state persistence
        // and the next storage transaction, which records send success.
        if self.crash_point == PrivateOperationCrashPoint::CiphertextPublished
            && self.prepared_committed.load(Ordering::SeqCst)
        {
            self.stop_at_crash_boundary().await;
        }

        let prepared_in_transaction = Arc::new(AtomicBool::new(false));
        let observed_prepared = Arc::clone(&prepared_in_transaction);
        let receive_in_transaction = Arc::new(AtomicBool::new(false));
        let observed_receive = Arc::clone(&receive_in_transaction);
        let result = self
            .inner
            .transaction_erased(Box::new(move |tx| {
                let before = tx.export_storage_state();
                let result = transaction(tx);
                if result.is_ok() && contains_new_prepared_send(&before, &tx.export_storage_state())
                {
                    observed_prepared.store(true, Ordering::SeqCst);
                }
                if result.is_ok()
                    && contains_new_private_stream_item(&before, &tx.export_storage_state())
                {
                    observed_receive.store(true, Ordering::SeqCst);
                }
                result
            }))
            .await?;

        if prepared_in_transaction.load(Ordering::SeqCst) {
            self.prepared_committed.store(true, Ordering::SeqCst);
            if self.crash_point == PrivateOperationCrashPoint::PreparedStateCommitted {
                self.stop_at_crash_boundary().await;
            }
        }
        if receive_in_transaction.load(Ordering::SeqCst)
            && self.crash_point == PrivateOperationCrashPoint::PrivateReceiveCheckpointCommitted
        {
            self.stop_at_crash_boundary().await;
        }
        Ok(result)
    }
}

fn contains_new_prepared_send(
    before: &paykit_sdk::storage::StorageState,
    after: &paykit_sdk::storage::StorageState,
) -> bool {
    after.outbound_private_messages.iter().any(|message| {
        message.prepared_send.is_some()
            && before
                .outbound_private_messages
                .iter()
                .find(|before| before.outbound_message_id == message.outbound_message_id)
                .is_some_and(|before| before.prepared_send.is_none())
    })
}

fn contains_new_private_stream_item(
    before: &paykit_sdk::storage::StorageState,
    after: &paykit_sdk::storage::StorageState,
) -> bool {
    after.private_stream_items.len() > before.private_stream_items.len()
}

fn test_app(name: &str) -> PaykitApp {
    PaykitApp::new(
        name,
        PaykitAppCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: true,
        },
    )
    .unwrap()
}

#[tokio::test]
async fn test_independent_apps_claim_and_handoff_one_payment_request() {
    let pair = linked_homeserver_shared_pair().await;
    let request = pair
        .bob
        .sdk
        .propose_payment_request(pair.bitkit.public_key.clone(), recurring_request_terms())
        .await
        .expect("request proposal should queue");
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("request proposal should send");
    pair.bitkit
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("request proposal should be received");

    let request_id = request_id(&request);
    let (bitkit_result, server_result) = tokio::join!(
        pair.bitkit
            .sdk
            .claim_payment_request_for_execution(pair.bob.public_key.clone(), &request_id),
        pair.server
            .sdk
            .claim_payment_request_for_execution(pair.bob.public_key.clone(), &request_id),
    );

    assert_ne!(bitkit_result.is_ok(), server_result.is_ok());
    let winner = bitkit_result
        .as_ref()
        .ok()
        .and_then(|record| record.execution_claim_app_id.clone())
        .or_else(|| {
            server_result
                .as_ref()
                .ok()
                .and_then(|record| record.execution_claim_app_id.clone())
        })
        .expect("one application should own payment execution");
    let record = request_with_id(
        pair.bitkit
            .sdk
            .payment_requests_with(&pair.bob.public_key)
            .await
            .expect("shared request state should remain readable"),
        request_id.as_str(),
    );
    assert_eq!(record.execution_claim_app_id, Some(winner.clone()));
    assert_eq!(record.state, PaymentRequestLifecycleState::Proposed);

    if winner == pair.bitkit.app_id {
        pair.bitkit
            .sdk
            .accept_payment_request(pair.bob.public_key.clone(), &request_id)
            .await
            .expect("the winning application should accept the request");
    } else {
        pair.server
            .sdk
            .accept_payment_request(pair.bob.public_key.clone(), &request_id)
            .await
            .expect("the winning application should accept the request");
    }

    let handoff = if winner == pair.bitkit.app_id {
        pair.bitkit
            .sdk
            .release_payment_request_execution_claim(pair.bob.public_key.clone(), &request_id)
            .await
            .expect("Bitkit should release recurring payment execution");
        pair.server
            .sdk
            .claim_payment_request_for_execution(pair.bob.public_key.clone(), &request_id)
            .await
            .expect("Paykit Server should claim the released subscription")
    } else {
        pair.server
            .sdk
            .release_payment_request_execution_claim(pair.bob.public_key.clone(), &request_id)
            .await
            .expect("Paykit Server should release recurring payment execution");
        pair.bitkit
            .sdk
            .claim_payment_request_for_execution(pair.bob.public_key.clone(), &request_id)
            .await
            .expect("Bitkit should claim the released subscription")
    };
    let next_owner = if winner == pair.bitkit.app_id {
        pair.server.app_id.clone()
    } else {
        pair.bitkit.app_id.clone()
    };
    assert_eq!(handoff.state, PaymentRequestLifecycleState::ActiveRecurring);
    assert_eq!(handoff.execution_claim_app_id, Some(next_owner));

    let snapshot = pair.bitkit.storage_state().await;
    let acceptance_count = snapshot
        .outbound_private_messages
        .iter()
        .filter(|message| message.kind == "paykit.payment_request_acceptance")
        .count();
    assert_eq!(acceptance_count, 1);
}

#[tokio::test]
async fn test_two_apps_share_private_request_state_and_app_lifecycle() {
    let pair = linked_two_party().await;
    let alice_server = pair
        .alice
        .additional_app(app_id("paykit-server"), "Paykit Server")
        .await;

    let request = pair
        .bob
        .sdk
        .propose_payment_request(pair.alice.public_key.clone(), recurring_request_terms())
        .await
        .expect("request proposal should queue");
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .expect("request proposal should send");

    let intake = pair
        .alice
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the first application should receive the request");
    assert_eq!(intake.stream_item_ids.len(), 1);

    let bitkit_request = request_with_id(
        pair.alice
            .sdk
            .payment_requests_with(&pair.bob.public_key)
            .await
            .expect("Bitkit should read shared request state"),
        &request.payment_request_id,
    );
    let server_request = request_with_id(
        alice_server
            .sdk
            .payment_requests_with(&pair.bob.public_key)
            .await
            .expect("Paykit Server should read shared request state"),
        &request.payment_request_id,
    );
    assert_eq!(bitkit_request, server_request);
    assert_eq!(server_request.payer_app_id, None);

    let second_intake = alice_server
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the second application should resume the shared receive checkpoint");
    assert!(second_intake.stream_item_ids.is_empty());

    let signed_out = pair
        .alice
        .sdk
        .sign_out()
        .await
        .expect("signing out one application should succeed");
    assert_eq!(signed_out.capability, PubkyIdentityCapability::SignedOut);

    alice_server
        .sdk
        .claim_payment_request_for_execution(pair.bob.public_key.clone(), &request_id(&request))
        .await
        .expect("the remaining application should claim payment execution");
    let accepted = alice_server
        .sdk
        .accept_payment_request(pair.bob.public_key.clone(), &request_id(&request))
        .await
        .expect("the remaining application should claim the payer response");
    assert_eq!(
        accepted.state,
        PaymentRequestLifecycleState::ActiveRecurring
    );
    assert_eq!(accepted.payer_app_id.as_ref(), Some(&alice_server.app_id));

    let other_app_cancel = pair
        .alice
        .sdk
        .cancel_payment_request(pair.bob.public_key.clone(), &request_id(&request), None)
        .await;
    assert!(matches!(
        other_app_cancel,
        Err(PaykitSdkError::Policy { .. })
    ));

    alice_server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("acceptance should send from the owning application");
    pair.bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .expect("the payee should receive the acceptance");

    let removal = alice_server.sdk.remove_paykit_app().await;
    assert!(matches!(removal, Err(PaykitSdkError::Policy { .. })));

    alice_server
        .sdk
        .cancel_payment_request(
            pair.bob.public_key.clone(),
            &request_id(&request),
            Some("application removal".into()),
        )
        .await
        .expect("the owning application should cancel its request state");
    let undelivered_removal = alice_server.sdk.remove_paykit_app().await;
    assert!(matches!(
        undelivered_removal,
        Err(PaykitSdkError::Policy { .. })
    ));

    alice_server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("cancellation should send before application removal");
    pair.bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .expect("the payee should receive the cancellation");

    let registry = alice_server
        .sdk
        .remove_paykit_app()
        .await
        .expect("application removal should succeed after owned work is complete");
    assert!(!registry.apps().contains_key(&alice_server.app_id));
    assert!(registry.apps().contains_key(&pair.alice.app_id));
}

#[tokio::test]
async fn test_failed_app_removal_is_isolated_and_retryable() {
    let pair = linked_two_party().await;
    let alice_server = pair
        .alice
        .additional_app(app_id("paykit-server"), "Paykit Server")
        .await;
    alice_server
        .sdk
        .enqueue_private_payment_list_with_reservations(
            pair.bob.public_key.clone(),
            vec![PrivatePaymentEndpointReservation {
                reservation_id: "server-reservation".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "ln-server".into(),
                },
                expires_at: None,
                attribution: std::collections::HashMap::new(),
            }],
        )
        .await
        .expect("server reservation should queue");
    let before = alice_server.storage.snapshot().unwrap();
    alice_server.adapter.set_fail_reservation_cancellation(true);

    let failed = alice_server.sdk.remove_paykit_app().await;

    assert!(matches!(failed, Err(PaykitSdkError::Policy { .. })));
    let after_failure = alice_server.storage.snapshot().unwrap();
    assert!(after_failure
        .registered_paykit_apps
        .contains(&pair.alice.app_id));
    assert!(!after_failure
        .retired_paykit_apps
        .contains(&pair.alice.app_id));
    assert_eq!(
        before
            .public_endpoint_records
            .iter()
            .filter(|((app_id, _), _)| app_id == &pair.alice.app_id)
            .collect::<Vec<_>>(),
        after_failure
            .public_endpoint_records
            .iter()
            .filter(|((app_id, _), _)| app_id == &pair.alice.app_id)
            .collect::<Vec<_>>()
    );

    alice_server
        .adapter
        .set_fail_reservation_cancellation(false);
    alice_server
        .storage
        .transaction({
            let counterparty = pair.bob.public_key.clone();
            let app_id = alice_server.app_id.clone();
            move |tx| {
                let mut reservation = tx
                    .payment_endpoint_reservation(&counterparty, &app_id, "server-reservation")
                    .expect("failed cleanup should preserve reservation");
                reservation.cancellation_started_at =
                    Some(Utc::now() - chrono::Duration::seconds(61));
                tx.save_payment_endpoint_reservation(reservation);
                Ok(())
            }
        })
        .await
        .unwrap();

    let registry = alice_server
        .sdk
        .remove_paykit_app()
        .await
        .expect("removal should succeed when cleanup can be retried");
    assert!(!registry.apps().contains_key(&alice_server.app_id));
    assert!(registry.apps().contains_key(&pair.alice.app_id));
}

fn recurring_request_terms() -> PaymentRequestTerms {
    let starts_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    PaymentRequestTerms {
        amount: PaymentAmount {
            value: "0.001".into(),
            asset: "btc".into(),
        },
        payment_reference: PaymentReference::new("shared-identity-subscription").unwrap(),
        proposal_expires_at: None,
        recurrence: Some(Recurrence {
            every: 1,
            unit: RecurrenceUnit::Month,
            starts_at: starts_at.clone(),
            anchor: starts_at,
            ends_at: None,
        }),
        accepted_payment_endpoint_identifiers: vec![PaymentEndpointIdentifier::new(
            "btc-lightning-bolt11",
        )
        .unwrap()],
        required_app_id: None,
        metadata: JsonMap::new(),
    }
}

fn request_id(record: &paykit_sdk::PaymentRequestRecord) -> PaymentRequestId {
    PaymentRequestId::new(record.payment_request_id.clone()).unwrap()
}

fn request_with_id(
    records: Vec<paykit_sdk::PaymentRequestRecord>,
    payment_request_id: &str,
) -> paykit_sdk::PaymentRequestRecord {
    records
        .into_iter()
        .find(|record| record.payment_request_id == payment_request_id)
        .expect("shared Payment Request should exist")
}
