use std::{
    any::Any,
    sync::{
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use paykit_lib::{
    PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms, Recurrence, RecurrenceUnit,
};
use paykit_sdk::{
    storage::StorageTransactionCallback, LinkedPeerState, OutboundPrivateMessageStatus, PaykitApp,
    PaykitAppCapabilities, PaykitAppId, PaykitSdk, PaykitSdkConfig, PaykitSdkError,
    PaymentRequestLifecycleState, PrivatePaymentEndpointReservation, PrivateReceivingDetail,
    PubkyIdentityCapability, PubkyLocalSecretKey, PubkyPublicKey, PubkySessionAccess,
    PubkySessionBootstrap, PubkySharedStateStorage, Result as PaykitResult, StorageAdapter,
    PAYKIT_SESSION_CAPABILITIES,
};
use pubky_testnet::EphemeralTestnet;
use serde_json::Map as JsonMap;

use crate::harness::{
    app_id, build_testnet, linked_two_party, TestUser, TestnetPaymentAdapter,
    TestnetSessionProvider,
};

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
        PaykitSdkError::Storage { context, .. }
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
async fn test_homeserver_backed_apps_share_noise_state_under_concurrency() {
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
    let stale_send = stale_send.join().unwrap();
    assert!(is_shared_state_conflict(&stale_send));

    let retried_send = bitkit_send_sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the stale sender should reload shared state and retry safely");
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
    let stale_receive = stale_receive.join().unwrap();
    assert!(is_shared_state_conflict(&stale_receive));

    let retried_receive = bitkit_receive_sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the stale receiver should reload shared state and retry safely");
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

type SharedStateSdk =
    PaykitSdk<PubkySharedStateStorage, TestnetSessionProvider, TestnetPaymentAdapter>;
type PausedSharedStateSdk =
    PaykitSdk<OneShotPausedStorage, TestnetSessionProvider, TestnetPaymentAdapter>;

struct SharedStateTestUser {
    sdk: SharedStateSdk,
    storage: PubkySharedStateStorage,
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
        let sdk = PaykitSdk::new(
            storage.clone(),
            provider,
            TestnetPaymentAdapter::default(),
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

    async fn storage_state(&self) -> paykit_sdk::storage::StorageState {
        self.storage
            .transaction(|tx| Ok(tx.export_storage_state()))
            .await
            .expect("shared state should remain readable")
    }
}

struct HomeserverSharedPair {
    _testnet: EphemeralTestnet,
    bitkit: SharedStateTestUser,
    server: SharedStateTestUser,
    bob: TestUser,
}

async fn linked_homeserver_shared_pair() -> HomeserverSharedPair {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let result = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap(), "paykit-sdk.test")
        .unwrap()
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .expect("shared identity sign-up should succeed");
    let bitkit = SharedStateTestUser::new(
        result.access.clone(),
        result.public_key.clone(),
        app_id("bitkit"),
        "Bitkit",
    )
    .await;
    let server = SharedStateTestUser::new(
        result.access,
        result.public_key,
        app_id("paykit-server"),
        "Paykit Server",
    )
    .await;
    let bob = TestUser::sign_up_with_app(&testnet, app_id("paykit-server")).await;

    bitkit
        .sdk
        .initiate_link_with_peer(bob.public_key.clone())
        .await
        .expect("shared identity should initiate the Encrypted Link Handshake");
    bob.sdk
        .accept_link_with_peer(bitkit.public_key.clone())
        .await
        .expect("the peer should accept the Encrypted Link Handshake");
    drive_shared_link_to_linked(&bitkit, &bob).await;

    HomeserverSharedPair {
        _testnet: testnet,
        bitkit,
        server,
        bob,
    }
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

fn is_shared_state_conflict<T>(result: &PaykitResult<T>) -> bool {
    matches!(
        result,
        Err(PaykitSdkError::Storage { context, .. })
            if context.contains("changed during transaction")
    )
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
async fn test_two_apps_concurrently_claim_one_payment_request_response() {
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
    pair.alice
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("request proposal should be received");

    let request_id = request_id(&request);
    let (bitkit_result, server_result) = tokio::join!(
        pair.alice
            .sdk
            .accept_payment_request(pair.bob.public_key.clone(), &request_id),
        alice_server
            .sdk
            .accept_payment_request(pair.bob.public_key.clone(), &request_id),
    );

    assert_ne!(bitkit_result.is_ok(), server_result.is_ok());
    let winner = bitkit_result
        .as_ref()
        .ok()
        .and_then(|record| record.payer_app_id.clone())
        .or_else(|| {
            server_result
                .as_ref()
                .ok()
                .and_then(|record| record.payer_app_id.clone())
        })
        .expect("one application should own the accepted request");
    let record = request_with_id(
        pair.alice
            .sdk
            .payment_requests_with(&pair.bob.public_key)
            .await
            .expect("shared request state should remain readable"),
        request_id.as_str(),
    );
    assert_eq!(record.payer_app_id, Some(winner));

    let snapshot = pair.alice.storage.snapshot().unwrap();
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
