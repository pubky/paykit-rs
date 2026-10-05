use std::{
    any::Any,
    future::pending,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use paykit_lib::{
    BillingPeriod, PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms, PaymentRequestTermsBuilder, Recurrence, RecurrenceConfig, RecurrenceUnit,
};
use paykit_sdk::{
    storage::{StorageOperation, StorageTransactionCallback},
    Clock, ContactUpdate, InMemoryStorage, LinkedPeerState, OutboundPrivateMessageStatus,
    PaykitApp, PaykitAppCapabilities, PaykitAppId, PaykitSdk, PaykitSdkConfig, PaykitSdkError,
    PaymentRequestLifecycleState, PrivatePaymentEndpointReservation,
    PrivatePaymentListReservationUpdate, PrivateReceivingDetail, PubkyIdentityCapability,
    PubkyLocalSecretKey, PubkyPublicKey, PubkySessionAccess, PubkySessionBootstrap,
    PubkySharedStateStorage, ReceiptDraftBuilder, ReceiptIssuanceStatus, Result as PaykitResult,
    StorageAdapter, PAYKIT_AUTHORIZER_SESSION_CAPABILITIES, PAYKIT_SESSION_CAPABILITIES,
};
use serde_json::Map as JsonMap;
use tokio::sync::oneshot;

use crate::harness::{
    app_id, build_testnet, build_testnet_with_admin, linked_two_party, private_receiving_detail,
    session_bootstrap, TestUser, TestnetInstance, TestnetPaymentAdapter, TestnetSessionProvider,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_independent_apps_register_concurrently_without_lost_updates() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let bitkit_result = session_bootstrap(&testnet, "bitkit.test")
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
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
    bitkit
        .publish_paykit_noise_key_authorization()
        .await
        .unwrap();

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
async fn test_failed_app_publication_keeps_capability_restrictions_until_republished() {
    let pair = homeserver_shared_pair().await;
    let user = &pair.bitkit;
    let mut capabilities = test_app("Bitkit").capabilities();
    capabilities.receipts = false;
    let app = PaykitApp::new("Bitkit", capabilities).unwrap();
    let remote = user.access.session.storage();
    let lock = remote
        .lock(
            paykit_lib::PAYKIT_APP_REGISTRY_PATH,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let error = user.sdk.publish_paykit_app(app.clone()).await.unwrap_err();
    assert!(error.is_concurrent_update());
    assert_eq!(
        user.storage
            .transaction(|tx| Ok(tx.paykit_app_capabilities(&user.app_id)))
            .await
            .unwrap(),
        Some(capabilities),
        "publication failure must not restore wider capabilities"
    );
    assert!(user
        .storage_state()
        .await
        .paykit_app_operation_leases
        .is_empty());
    remote.unlock(&lock).await.unwrap();

    let registry = user.sdk.publish_paykit_app(app).await.unwrap();
    assert_eq!(
        registry.apps().get(&user.app_id).unwrap().capabilities(),
        capabilities
    );
    let restored = test_app("Bitkit");
    user.sdk.publish_paykit_app(restored.clone()).await.unwrap();
    assert_eq!(
        user.storage
            .transaction(|tx| Ok(tx.paykit_app_capabilities(&user.app_id)))
            .await
            .unwrap(),
        Some(restored.capabilities())
    );
}

#[tokio::test]
async fn test_unchanged_app_publication_initializes_noise_key() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let result = session_bootstrap(&testnet, "bitkit.test")
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
        .await
        .unwrap();
    let storage = InMemoryStorage::new();
    let mut public_only_access = result.access.clone();
    public_only_access.local_secret_key = None;
    let app = PaykitApp::new(
        "Bitkit",
        PaykitAppCapabilities {
            private_payments: false,
            payment_requests: false,
            receipts: false,
            outgoing_payments: false,
        },
    )
    .unwrap();
    let public_only = PaykitSdk::new(
        storage.clone(),
        TestnetSessionProvider::new(public_only_access),
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    public_only.initialize().await.unwrap();
    let registry = public_only.publish_paykit_app(app.clone()).await.unwrap();
    assert!(registry.noise_public_key().is_none());

    let mut private_access = result.access;
    private_access.paykit_identity_secret_key =
        Some(secret.derive_paykit_identity_secret_key(3).unwrap());
    let private_capable = PaykitSdk::new(
        storage,
        TestnetSessionProvider::new(private_access),
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    private_capable.initialize().await.unwrap();
    private_capable
        .publish_paykit_noise_key_authorization()
        .await
        .unwrap();
    let published = private_capable.publish_paykit_app(app).await.unwrap();
    assert!(published.noise_public_key().is_some());
    assert_eq!(published.key_generation(), 3);
    let fetched = private_capable
        .paykit_app_registry(result.public_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched, published);
}

#[tokio::test]
async fn test_pubky_shared_state_is_visible_to_independent_apps_and_survives_sign_out() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let bitkit_result =
        PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap(), "paykit-sdk.test")
            .unwrap()
            .sign_up(
                &secret,
                &homeserver,
                None,
                PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
            )
            .await
            .unwrap();
    let bitkit_session_secret = bitkit_result
        .export_session_secret()
        .await
        .unwrap()
        .into_inner();
    let access = bitkit_result.access;

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

    let bitkit_provider =
        TestnetSessionProvider::with_session_secret(access.clone(), bitkit_session_secret);
    let bitkit = PaykitSdk::new(
        PubkySharedStateStorage::new(bitkit_provider.clone()),
        bitkit_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    bitkit.initialize().await.unwrap();
    bitkit
        .publish_paykit_noise_key_authorization()
        .await
        .unwrap();
    bitkit.publish_paykit_app(test_app("Bitkit")).await.unwrap();
    assert_no_pending_shared_state_writes(&access.session).await;

    let server_access = session_bootstrap(&testnet, "paykit-server.test")
        .sign_in(&secret, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    let server_session = server_access.session.clone();
    let server_provider = TestnetSessionProvider::new(server_access);
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
    assert_no_pending_shared_state_writes(&server_session).await;
}

struct CountedStorage {
    inner: PubkySharedStateStorage,
    transactions: Arc<AtomicUsize>,
}

#[async_trait]
impl StorageAdapter for CountedStorage {
    async fn run_operation_erased<'a>(
        &self,
        operation: StorageOperation<'a>,
    ) -> PaykitResult<Box<dyn Any + Send>> {
        self.inner.run_operation_erased(operation).await
    }

    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> PaykitResult<Box<dyn Any + Send>> {
        self.transactions.fetch_add(1, Ordering::SeqCst);
        self.inner.transaction_erased(f).await
    }
}

#[tokio::test]
async fn test_shared_storage_operation_keeps_committed_transactions_after_error() {
    let pair = homeserver_shared_pair().await;
    let storage = &pair.bitkit.storage;
    let remote = pair.bitkit.access.session.storage();
    let before = storage.load_identity_state().await.unwrap().unwrap();
    let mut expected = before.clone();
    expected.initialized_at += chrono::Duration::seconds(1);

    let error = storage
        .with_operation(async {
            storage.save_identity_state(expected.clone()).await?;
            let committed = remote
                .get(paykit_lib::PAYKIT_SHARED_STATE_PATH)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            storage
                .with_operation(async {
                    assert_eq!(storage.load_identity_state().await?, Some(expected.clone()));
                    let rejected = storage
                        .transaction::<(), _>(|tx| {
                            tx.save_identity_state(before.clone());
                            Err(PaykitSdkError::Policy {
                                context: "reject transaction".into(),
                                source: None,
                            })
                        })
                        .await;
                    assert!(rejected.is_err());
                    assert_eq!(storage.load_identity_state().await?, Some(expected.clone()));
                    Ok(())
                })
                .await?;
            assert!(matches!(
                pair.bitkit.sdk.identity_status().await,
                Err(PaykitSdkError::Policy { .. })
            ));
            assert!(matches!(
                pair.bitkit.sdk.publish_paykit_app(test_app("Bitkit")).await,
                Err(PaykitSdkError::Policy { .. })
            ));
            assert!(matches!(
                pair.server.storage.load_identity_state().await,
                Err(PaykitSdkError::Policy { .. })
            ));
            assert_eq!(
                remote
                    .get(paykit_lib::PAYKIT_SHARED_STATE_PATH)
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap(),
                committed,
            );
            assert!(matches!(
                remote.lock(paykit_lib::PAYKIT_SHARED_STATE_PATH, Duration::from_secs(60)).await,
                Err(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
                    if status == pubky::StatusCode::LOCKED
            ));
            Err::<(), _>(PaykitSdkError::Policy {
                context: "stop after committed transaction".into(),
                source: None,
            })
        })
        .await
        .unwrap_err();
    assert!(matches!(error, PaykitSdkError::Policy { .. }));
    assert_eq!(
        pair.server.storage.load_identity_state().await.unwrap(),
        Some(expected)
    );
    pair.server
        .storage
        .save_identity_state(before.clone())
        .await
        .unwrap();
    assert_eq!(storage.load_identity_state().await.unwrap(), Some(before));
    assert_no_pending_shared_state_writes(&pair.bitkit.access.session).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_wallet_reservation_cleanup_does_not_hold_shared_state_lock() {
    let pair = linked_homeserver_shared_pair().await;
    pair.bitkit
        .sdk
        .enqueue_private_payment_list_with_reservations(
            pair.bob.public_key.clone(),
            vec![PrivatePaymentEndpointReservation {
                reservation_id: "reserved-invoice".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "ln-invoice".into(),
                },
                expires_at: None,
                attribution: std::collections::HashMap::new(),
            }],
        )
        .await
        .unwrap();
    pair.bitkit
        .storage
        .transaction(|tx| {
            let mut reservation = tx
                .payment_endpoint_reservation(
                    &pair.bob.public_key,
                    &pair.bitkit.app_id,
                    "reserved-invoice",
                )
                .unwrap();
            reservation.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
            tx.save_payment_endpoint_reservation(reservation);
            Ok(())
        })
        .await
        .unwrap();
    let (paused, resume) = pair.bitkit.adapter.pause_next_reservation_cancellation();
    let send = pair
        .bitkit
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone());
    let observe = async {
        tokio::time::timeout(Duration::from_secs(10), paused)
            .await
            .unwrap()
            .unwrap();
        let read = pair.server.storage.load_identity_state().await;
        resume.send(()).unwrap();
        assert!(read.unwrap().is_some());
    };
    let (sent, ()) = tokio::join!(send, observe);
    let sent = sent.unwrap();
    assert_eq!(sent.failed.len(), 1);
    assert!(sent.sent.is_empty());
    assert!(sent.reservation_cleanup_failures.is_empty());
    assert!(pair
        .bitkit
        .storage_state()
        .await
        .payment_endpoint_reservations
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_private_list_clear_and_app_publication_transaction_counts() {
    let pair = linked_homeserver_shared_pair().await;
    let transactions = Arc::new(AtomicUsize::new(0));
    let sdk = PaykitSdk::new(
        CountedStorage {
            inner: pair.bitkit.storage.clone(),
            transactions: transactions.clone(),
        },
        TestnetSessionProvider::new(pair.bitkit.access.clone()),
        pair.bitkit.adapter.clone(),
        PaykitSdkConfig::new(pair.bitkit.app_id.clone()).unwrap(),
    );
    let cleared = sdk
        .clear_private_payment_list_and_process_outbound(pair.bob.public_key.clone())
        .await
        .unwrap();
    let clear_transactions = transactions.swap(0, Ordering::SeqCst);
    assert_eq!(clear_transactions, 14);
    assert_eq!(cleared.cleared.len(), 1);
    assert!(cleared.failed_to_queue.is_empty());
    assert!(cleared.failed_to_deliver.is_empty());
    let state = pair.bitkit.storage_state().await;
    let sent = state
        .outbound_private_messages
        .iter()
        .find(|message| Some(message.outbound_message_id) == cleared.cleared[0].outbound_message_id)
        .unwrap();
    assert_eq!(sent.status, OutboundPrivateMessageStatus::Sent);
    assert!(sent.prepared_send.is_none());
    assert!(state.peer_link_operation_leases.is_empty());
    pair.bob
        .sdk
        .receive_private_messages(pair.bitkit.public_key.clone())
        .await
        .unwrap();
    let lists = pair
        .bob
        .sdk
        .current_private_payment_lists(&pair.bitkit.public_key)
        .await
        .unwrap();
    assert!(lists
        .iter()
        .any(|list| list.app_id == pair.bitkit.app_id && list.payment_endpoints.is_empty()));

    let proposal = sdk
        .propose_payment_request(pair.bob.public_key.clone(), recurring_request_terms())
        .await
        .unwrap();
    let proposal_transactions = transactions.swap(0, Ordering::SeqCst);
    assert_eq!(proposal_transactions, 2);
    let sent = sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    let send_transactions = transactions.swap(0, Ordering::SeqCst);
    assert!(
        send_transactions <= 11,
        "request send used {send_transactions} transactions"
    );
    assert_eq!(sent.sent.len(), 1);
    assert!(sent.failed.is_empty());
    pair.bob
        .sdk
        .receive_private_messages(pair.bitkit.public_key.clone())
        .await
        .unwrap();
    assert!(pair
        .bob
        .sdk
        .received_payment_requests_from(&pair.bitkit.public_key)
        .await
        .unwrap()
        .iter()
        .any(|request| request.payment_request_id == proposal.payment_request_id));

    sdk.publish_paykit_app(test_app("Bitkit")).await.unwrap();
    let unchanged_transactions = transactions.swap(0, Ordering::SeqCst);
    assert_eq!(unchanged_transactions, 7);
    let mut capabilities = test_app("Bitkit").capabilities();
    capabilities.private_payments = false;
    let registry = sdk
        .publish_paykit_app(PaykitApp::new("Bitkit", capabilities).unwrap())
        .await
        .unwrap();
    let downgrade_transactions = transactions.swap(0, Ordering::SeqCst);
    assert_eq!(downgrade_transactions, 8);
    assert_eq!(
        registry
            .apps()
            .get(&pair.bitkit.app_id)
            .unwrap()
            .capabilities(),
        capabilities
    );
    let state = pair.bitkit.storage_state().await;
    assert_eq!(
        state
            .registered_paykit_app_capabilities
            .get(&pair.bitkit.app_id),
        Some(&capabilities)
    );
    assert!(state.paykit_app_operation_leases.is_empty());
    assert_no_pending_shared_state_writes(&pair.bitkit.access.session).await;
    eprintln!("shared-state transactions: clear={clear_transactions}, proposal={proposal_transactions}, send={send_transactions}, unchanged publication={unchanged_transactions}, downgrade={downgrade_transactions}");
}

#[tokio::test]
async fn test_app_publication_rechecks_registry_and_authorization_after_staging() {
    struct ChangedAfterStagingStorage {
        inner: PubkySharedStateStorage,
        access: PubkySessionAccess,
        remove_authorization: bool,
        staged_changes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageAdapter for ChangedAfterStagingStorage {
        async fn run_operation_erased<'a>(
            &self,
            operation: StorageOperation<'a>,
        ) -> PaykitResult<Box<dyn Any + Send>> {
            self.inner.run_operation_erased(operation).await
        }

        async fn transaction_erased<'a>(
            &self,
            f: StorageTransactionCallback<'a>,
        ) -> PaykitResult<Box<dyn Any + Send>> {
            let mut staged = false;
            let result = self
                .inner
                .transaction_erased(Box::new(|tx| {
                    let previous = tx.paykit_app_capabilities(&app_id("bitkit"));
                    let result = f(tx)?;
                    staged = previous.is_some_and(|capabilities| capabilities.receipts)
                        && tx
                            .paykit_app_capabilities(&app_id("bitkit"))
                            .is_some_and(|capabilities| !capabilities.receipts);
                    Ok(result)
                }))
                .await?;
            if staged {
                self.staged_changes.fetch_add(1, Ordering::SeqCst);
                if self.remove_authorization {
                    self.access
                        .session
                        .storage()
                        .delete(paykit_lib::PAYKIT_NOISE_KEY_AUTHORIZATION_PATH)
                        .await
                        .unwrap();
                } else {
                    let (mut registry, revision) =
                        paykit_lib::get_paykit_app_registry_with_revision(
                            &self.access.outbox_client.public_storage(),
                            self.access.session.info().public_key(),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                    registry
                        .register_app(app_id("other"), test_app("Other"))
                        .unwrap();
                    paykit_lib::update_paykit_app_registry(
                        &self.access.session,
                        &registry,
                        &revision,
                    )
                    .await
                    .unwrap();
                }
            }
            Ok(result)
        }
    }

    for remove_authorization in [false, true] {
        let pair = homeserver_shared_pair().await;
        let user = &pair.bitkit;
        let staged_changes = Arc::new(AtomicUsize::new(0));
        let sdk = PaykitSdk::new(
            ChangedAfterStagingStorage {
                inner: user.storage.clone(),
                access: user.access.clone(),
                remove_authorization,
                staged_changes: staged_changes.clone(),
            },
            TestnetSessionProvider::new(user.access.clone()),
            user.adapter.clone(),
            PaykitSdkConfig::new(user.app_id.clone()).unwrap(),
        );
        let mut capabilities = test_app("Bitkit").capabilities();
        capabilities.receipts = false;
        let result = sdk
            .publish_paykit_app(PaykitApp::new("Bitkit", capabilities).unwrap())
            .await;
        assert_eq!(staged_changes.load(Ordering::SeqCst), 1);
        let registry = user
            .sdk
            .paykit_app_registry(user.public_key.clone())
            .await
            .unwrap()
            .unwrap();
        if remove_authorization {
            assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
            assert!(
                registry
                    .apps()
                    .get(&user.app_id)
                    .unwrap()
                    .capabilities()
                    .receipts
            );
        } else {
            assert_eq!(result.unwrap(), registry);
            assert_eq!(
                registry.apps().get(&app_id("other")),
                Some(&test_app("Other"))
            );
            assert_eq!(
                registry.apps().get(&user.app_id).unwrap().capabilities(),
                capabilities
            );
        }
        let state = user.storage_state().await;
        assert_eq!(
            state.registered_paykit_app_capabilities.get(&user.app_id),
            Some(&capabilities)
        );
        assert!(state.paykit_app_operation_leases.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_idle_polling_reads_shared_state_once_without_rewriting_it() {
    let pair = linked_homeserver_shared_pair().await;
    pair.bob
        .sdk
        .propose_payment_request(pair.bitkit.public_key.clone(), recurring_request_terms())
        .await
        .unwrap();
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.bitkit.public_key.clone())
        .await
        .unwrap();
    pair.bitkit
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.bitkit
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    let storage = pair.bitkit.access.session.storage();
    let before = storage
        .get(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    for user in [&pair.bitkit, &pair.server] {
        let transactions = Arc::new(AtomicUsize::new(0));
        let sdk = PaykitSdk::new(
            CountedStorage {
                inner: user.storage.clone(),
                transactions: transactions.clone(),
            },
            TestnetSessionProvider::new(user.access.clone()),
            user.adapter.clone(),
            PaykitSdkConfig::new(user.app_id.clone()).unwrap(),
        );
        let report = sdk
            .receive_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap();
        assert_eq!(report.receive_batch_id, None);
        assert!(report.stream_item_ids.is_empty());
        let single_transactions = transactions.swap(0, Ordering::SeqCst);
        let reports = sdk
            .receive_private_messages_from_linked_peers()
            .await
            .unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].report, Some(report));
        assert!(reports[0].error.is_none());
        let batch_transactions = transactions.swap(0, Ordering::SeqCst);
        assert_eq!(single_transactions, 1);
        assert_eq!(batch_transactions, 1);
        let sent = sdk
            .process_outbound_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap();
        assert!(sent.attempted.is_empty());
        assert_eq!(transactions.swap(0, Ordering::SeqCst), 1);
        let requests = sdk.payment_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].counterparty, pair.bob.public_key);
        assert_eq!(transactions.load(Ordering::SeqCst), 1);
        assert_no_pending_shared_state_writes(&user.access.session).await;
    }
    let after = storage
        .get(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "idle polling must not rewrite encrypted state"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_shared_state_compacts_private_lists_without_changing_read_only_state() {
    let pair = linked_homeserver_shared_pair().await;
    for user in [&pair.bitkit, &pair.server] {
        for details in [
            vec![private_receiving_detail(
                "btc-lightning-bolt11",
                "ln-private",
            )],
            Vec::new(),
        ] {
            user.sdk
                .enqueue_private_payment_list_with_receiving_details(
                    pair.bob.public_key.clone(),
                    details,
                )
                .await
                .unwrap();
            let report = user
                .sdk
                .process_outbound_private_messages(pair.bob.public_key.clone())
                .await
                .unwrap();
            assert_eq!(report.sent.len(), 1);
            assert!(report.failed.is_empty());
        }
    }
    for details in [
        vec![private_receiving_detail("btc-lightning-bolt11", "ln-bob")],
        Vec::new(),
    ] {
        pair.bob
            .sdk
            .enqueue_private_payment_list_with_receiving_details(
                pair.bitkit.public_key.clone(),
                details,
            )
            .await
            .unwrap();
        let report = pair
            .bob
            .sdk
            .process_outbound_private_messages(pair.bitkit.public_key.clone())
            .await
            .unwrap();
        assert_eq!(report.sent.len(), 1);
        assert!(report.failed.is_empty());
        pair.bitkit
            .sdk
            .receive_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap();
    }
    let views = pair
        .server
        .sdk
        .current_private_payment_lists(&pair.bob.public_key)
        .await
        .unwrap();
    assert_eq!(views.len(), 1);
    assert!(views[0].payment_endpoints.is_empty());
    let state = pair.server.storage_state().await;
    let kind = paykit_lib::PrivateMessageKind::PrivatePaymentList.as_str();
    let sent = state
        .outbound_private_messages
        .iter()
        .filter(|record| record.kind == kind)
        .collect::<Vec<_>>();
    assert_eq!(sent.len(), 2);
    for record in sent {
        assert_eq!(record.status, OutboundPrivateMessageStatus::Sent);
        assert!(
            paykit_lib::parse_private_payment_list_json(&record.raw_json)
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        state
            .private_stream_items
            .iter()
            .filter(|record| record.known_paykit_kind.as_deref() == Some(kind))
            .count(),
        1
    );
    let revision = pair.server.storage.last_revision().unwrap();
    assert_eq!(pair.server.storage_state().await, state);
    assert_eq!(pair.server.storage.last_revision().unwrap(), revision);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_same_app_devices_serialize_public_endpoint_sync() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let first_result = session_bootstrap(&testnet, "bitkit-first.test")
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
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
        let records = first
            .storage
            .transaction(|tx| Ok(tx.public_endpoint_records()))
            .await
            .expect("wallet callbacks must leave shared storage accessible");
        assert!(records.is_empty());
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

    let records = second.storage_state().await.public_endpoint_records;
    let unchanged = second.sdk.sync_public_endpoints().await.unwrap();
    assert_eq!(unchanged.published.len(), 1);
    assert!(unchanged.failed.is_empty());
    assert_eq!(
        second.storage_state().await.public_endpoint_records,
        records
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
async fn test_handshake_advancement_checks_peer_state_after_claiming_lease() {
    let pair = homeserver_shared_pair().await;
    let counterparty = &pair.bob.public_key;
    pair.bitkit
        .sdk
        .initiate_link_with_peer(counterparty.clone())
        .await
        .unwrap();

    // Hold the state between observing a new marker and saving its handshake.
    let lease = pair
        .bitkit
        .storage
        .transaction(|tx| {
            let now = Utc::now();
            let lease = tx
                .claim_peer_link_operation(counterparty, now, now + chrono::Duration::minutes(1))?
                .unwrap();
            let mut peer = tx.linked_peer(counterparty).unwrap();
            peer.state = LinkedPeerState::RecoveryRequired;
            tx.save_linked_peer(peer);
            Ok(lease)
        })
        .await
        .unwrap();

    let result = pair
        .server
        .sdk
        .advance_link_handshake(counterparty.clone())
        .await;
    assert!(result.unwrap_err().is_concurrent_update());
    pair.bitkit
        .storage
        .transaction(|tx| {
            assert_eq!(
                tx.peer_link_operation_lease(counterparty),
                Some(lease.clone())
            );
            tx.release_peer_link_operation(counterparty, lease.lease_id);
            Ok(())
        })
        .await
        .unwrap();

    let result = pair
        .server
        .sdk
        .advance_link_handshake(counterparty.clone())
        .await;
    assert!(matches!(
        result,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
    pair.server
        .storage
        .transaction(|tx| {
            assert!(tx.peer_link_operation_lease(counterparty).is_none());
            assert_eq!(
                tx.linked_peer(counterparty).unwrap().state,
                LinkedPeerState::RecoveryRequired
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_private_receive_checks_peer_state_after_claiming_lease() {
    let pair = homeserver_shared_pair().await;
    let counterparty = &pair.bob.public_key;
    pair.bitkit
        .sdk
        .initiate_link_with_peer(counterparty.clone())
        .await
        .unwrap();

    let lease = pair
        .bitkit
        .storage
        .transaction(|tx| {
            let now = Utc::now();
            Ok(tx
                .claim_peer_link_operation(counterparty, now, now + chrono::Duration::minutes(1))?
                .unwrap())
        })
        .await
        .unwrap();
    let result = pair
        .server
        .sdk
        .receive_private_messages(counterparty.clone())
        .await;
    assert!(result.unwrap_err().is_concurrent_update());
    pair.bitkit
        .storage
        .transaction(|tx| {
            assert_eq!(
                tx.peer_link_operation_lease(counterparty),
                Some(lease.clone())
            );
            tx.release_peer_link_operation(counterparty, lease.lease_id);
            Ok(())
        })
        .await
        .unwrap();

    let before = pair.server.storage_state().await;
    assert_eq!(
        before.linked_peers[counterparty].state,
        LinkedPeerState::Linking
    );
    assert!(before.encrypted_link_states[counterparty]
        .handshake_snapshot
        .is_some());
    let result = pair
        .server
        .sdk
        .receive_private_messages(counterparty.clone())
        .await;
    assert!(matches!(
        result,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
    let after = pair.server.storage_state().await;
    assert_eq!(after.linked_peers, before.linked_peers);
    assert_eq!(after.encrypted_link_states, before.encrypted_link_states);
    assert!(after.peer_link_operation_leases.is_empty());
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
            result.is_ok()
                || matches!(
                    result,
                    Err(PaykitSdkError::Policy { .. } | PaykitSdkError::ConcurrentUpdate { .. })
                ),
            "concurrent handshake advancement should advance or observe contention: {result:?}"
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
            result.is_ok()
                || matches!(
                    result,
                    Err(PaykitSdkError::Policy { .. } | PaykitSdkError::ConcurrentUpdate { .. })
                ),
            "concurrent recovery should observe the marker or contention: {result:?}"
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
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
        .await
        .unwrap()
        .access;
    let session_secret = access
        .session
        .as_grant()
        .unwrap()
        .export_local_secret()
        .await
        .unwrap();
    let provider =
        TestnetSessionProvider::with_session_secret(access.clone(), session_secret.clone());
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
    sdk.publish_paykit_noise_key_authorization().await.unwrap();
    sdk.publish_paykit_app(test_app("Bitkit")).await.unwrap();

    let invalid = paykit_sdk::PaykitIdentitySecretKey::new([42; 32], 2).unwrap();
    let revision = storage.last_revision().unwrap();
    assert!(matches!(
        sdk.rotate_paykit_identity_key(invalid).await,
        Err(PaykitSdkError::Identity { .. })
    ));
    assert_eq!(storage.last_revision().unwrap(), revision);

    let replacement = secret
        .derive_paykit_identity_secret_key(2)
        .expect("replacement Paykit key derivation should succeed");
    let registry = sdk
        .rotate_paykit_identity_key(replacement.clone())
        .await
        .expect("Paykit key rotation should succeed");
    assert_eq!(registry.key_generation(), 2);
    assert_no_pending_shared_state_writes(&access.session).await;

    let old_key_error = storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .expect_err("the previous Paykit key must not decrypt rotated state");
    assert!(matches!(old_key_error, PaykitSdkError::Identity { .. }));
    sdk.sign_out()
        .await
        .expect("old-key app must still be able to revoke its grant");
    assert!(testnet
        .sdk()
        .unwrap()
        .restore_session(&session_secret)
        .await
        .is_err());

    let mut replacement_access = session_bootstrap(&testnet, "replacement.test")
        .sign_in(&secret, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    replacement_access.paykit_identity_secret_key = Some(replacement);
    replacement_access.local_secret_key = None;
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

#[tokio::test]
async fn test_key_rotation_compacts_obsolete_private_lists() {
    let pair = linked_homeserver_shared_pair().await;
    let access = &pair.bitkit.access;
    let root = access.local_secret_key.as_ref().unwrap();
    let current = root.derive_paykit_identity_secret_key(1).unwrap();
    let replacement = root.derive_paykit_identity_secret_key(2).unwrap();
    let mut queued = Vec::new();
    for _ in 0..3 {
        queued.push(
            pair.bitkit
                .sdk
                .enqueue_private_payment_list_with_receiving_details(
                    pair.bob.public_key.clone(),
                    Vec::new(),
                )
                .await
                .unwrap(),
        );
    }
    let latest = queued.last().unwrap().clone();
    pair.bitkit
        .storage
        .rotate_paykit_identity_key(current, replacement.clone(), |tx, already_rotated| {
            assert!(!already_rotated);
            for (mut record, status) in queued.into_iter().zip([
                OutboundPrivateMessageStatus::Invalid,
                OutboundPrivateMessageStatus::RecoveryRequired,
                OutboundPrivateMessageStatus::RecoveryRequired,
            ]) {
                record.status = status;
                record.last_error = Some("link recovery required".into());
                tx.save_outbound_private_message(record)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let mut access = access.clone();
    access.paykit_identity_secret_key = Some(replacement);
    let storage = PubkySharedStateStorage::new(TestnetSessionProvider::new(access));
    let state = storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    assert_eq!(state.outbound_private_messages.len(), 1);
    assert_eq!(
        state.outbound_private_messages[0].outbound_message_id,
        latest.outbound_message_id
    );
    assert_eq!(state.outbound_private_messages[0].raw_json, latest.raw_json);
}

#[tokio::test]
async fn test_recover_missing_shared_state_retries_without_overwriting_progress() {
    let pair = linked_homeserver_shared_pair().await;
    pair.bitkit
        .sdk
        .save_contact(ContactUpdate {
            public_key: pair.bob.public_key.clone(),
            label: Some("Bob".into()),
        })
        .await
        .unwrap();
    pair.bob
        .sdk
        .propose_payment_request(pair.bitkit.public_key.clone(), recurring_request_terms())
        .await
        .unwrap();
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.bitkit.public_key.clone())
        .await
        .unwrap();
    pair.bitkit
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.bitkit
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.bitkit
        .sdk
        .reconcile_allowance_accounting(paykit_sdk::AllowanceAccountingReconciliation {
            expected_revision: None,
            history: Default::default(),
            outcomes: Vec::new(),
            trusted_time: Utc::now(),
        })
        .await
        .unwrap();
    let (sender, prepared) = pair.bitkit.crashable_sdk(
        PrivateOperationCrashPoint::PreparedStateCommitted,
        Utc::now(),
    );
    let counterparty = pair.bob.public_key.clone();
    let mut sending =
        tokio::spawn(async move { sender.process_outbound_private_messages(counterparty).await });
    tokio::select! {
        result = prepared => result.unwrap(),
        result = &mut sending => panic!("send completed before preparing ciphertext: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(30)) => panic!("send did not reach preparation"),
    }
    sending.abort();
    assert!(sending.await.unwrap_err().is_cancelled());
    wait_for_shared_state_after_crash(&pair.bitkit.storage).await;
    let backup = pair.bitkit.sdk.export_backup_state().await.unwrap();
    assert!(!backup.encrypted_link_states.is_empty());
    assert!(!backup.private_stream_items.is_empty());
    assert!(backup
        .outbound_private_messages
        .iter()
        .any(|message| message.prepared_send.is_some()));
    let remote = pair.bitkit.access.session.storage();
    remote
        .delete(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
        .unwrap();
    assert!(pair
        .bitkit
        .sdk
        .restore_backup_state(backup.clone())
        .await
        .is_err());

    let replacement = pair.secret.derive_paykit_identity_secret_key(2).unwrap();
    let registry_lock = remote
        .lock(
            paykit_lib::PAYKIT_APP_REGISTRY_PATH,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let result = pair
        .bitkit
        .sdk
        .recover_shared_state_from_backup(backup.clone(), replacement.clone())
        .await;
    remote.unlock(&registry_lock).await.unwrap();
    assert!(result.unwrap_err().is_concurrent_update());
    let unchanged_registry = pair
        .bitkit
        .sdk
        .paykit_app_registry(pair.bitkit.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged_registry.key_generation(), 1);

    let mut replacement_access = pair.bitkit.access.clone();
    replacement_access.paykit_identity_secret_key = Some(replacement.clone());
    let replacement_storage =
        PubkySharedStateStorage::new(TestnetSessionProvider::new(replacement_access));
    let restored = replacement_storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    assert!(restored.encrypted_link_states.is_empty());
    assert!(restored
        .outbound_private_messages
        .iter()
        .all(|message| message.prepared_send.is_none()));
    assert_eq!(restored.private_stream_items, backup.private_stream_items);
    assert_eq!(
        restored
            .contact_records
            .get(&pair.bob.public_key)
            .unwrap()
            .label
            .as_deref(),
        Some("Bob")
    );
    assert_eq!(
        restored.linked_peers[&pair.bob.public_key].state,
        LinkedPeerState::RecoveryRequired
    );
    let accounting = restored.allowance_accounting.unwrap();
    assert!(accounting.requires_reconciliation);
    assert_ne!(
        accounting.epoch,
        backup.allowance_accounting.as_ref().unwrap().epoch
    );

    replacement_storage
        .transaction(|tx| {
            let mut contact = tx.contact_record(&pair.bob.public_key).unwrap();
            contact.label = Some("Updated after recovery".into());
            tx.save_contact_record(contact);
            Ok(())
        })
        .await
        .unwrap();
    let before_retry = replacement_storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    let revision = replacement_storage.last_revision().unwrap();
    assert_eq!(
        pair.bitkit
            .sdk
            .recover_shared_state_from_backup(backup.clone(), replacement.clone())
            .await
            .unwrap()
            .key_generation(),
        2
    );
    assert_eq!(
        replacement_storage
            .transaction(|tx| Ok(tx.export_storage_state()))
            .await
            .unwrap(),
        before_retry
    );
    assert_eq!(replacement_storage.last_revision().unwrap(), revision);
    assert_eq!(
        pair.bitkit
            .sdk
            .recover_shared_state_from_backup(backup, replacement)
            .await
            .unwrap()
            .key_generation(),
        2
    );
}

#[tokio::test]
async fn test_restore_with_replacement_key_discards_old_link_snapshots() {
    let pair = linked_homeserver_shared_pair().await;
    let backup = pair.bitkit.sdk.export_backup_state().await.unwrap();
    assert!(!backup.encrypted_link_states.is_empty());
    let mut access = pair.bitkit.access.clone();
    access.paykit_identity_secret_key =
        Some(pair.secret.derive_paykit_identity_secret_key(2).unwrap());
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::new(
        storage.clone(),
        TestnetSessionProvider::new(access),
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    let report = sdk.restore_backup_state(backup.clone()).await.unwrap();
    assert!(report
        .recovery_required_peers
        .contains(&pair.bob.public_key));
    let restored = storage.snapshot().unwrap();
    assert!(restored.encrypted_link_states.is_empty());
    assert_ne!(
        restored.paykit_noise_public_key,
        backup.paykit_noise_public_key
    );
    assert_eq!(restored.private_stream_items, backup.private_stream_items);
    assert!(restored
        .outbound_private_messages
        .iter()
        .all(|message| message.prepared_send.is_none()));
}

#[tokio::test]
async fn test_unknown_pending_write_blocks_shared_state() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up_with_app(&testnet, app_id("bitkit")).await;
    let storage = PubkySharedStateStorage::new(TestnetSessionProvider::new(user.access.clone()));
    let remote = user.access.session.storage();
    let marker = format!(
        "{}unknown",
        paykit_lib::PAYKIT_SHARED_STATE_WRITE_PATH_PREFIX
    );
    remote.put(&marker, Vec::<u8>::new()).await.unwrap();
    let lock = remote
        .lock(
            paykit_lib::PAYKIT_SHARED_STATE_PATH,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let callback_ran = AtomicBool::new(false);
    let error = storage
        .transaction(|_| {
            callback_ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(matches!(error, PaykitSdkError::SharedStateBusy { .. }));
    remote.unlock(&lock).await.unwrap();
    let error = storage
        .transaction(|_| {
            callback_ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(matches!(error, PaykitSdkError::Storage { .. }));
    assert!(!callback_ran.load(Ordering::SeqCst));
    assert!(remote.get(&marker).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pubky_shared_state_waits_for_abandoned_write_before_reading() {
    let testnet = build_testnet_with_admin().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = session_bootstrap(&testnet, "bitkit.test")
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
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
    let remote = access.session.storage();
    let original = remote
        .get(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let late_initialized_at = storage
        .transaction(|tx| {
            let mut identity = tx.load_identity_state().unwrap();
            identity.initialized_at += chrono::Duration::seconds(1);
            let initialized_at = identity.initialized_at;
            tx.save_identity_state(identity);
            Ok(initialized_at)
        })
        .await
        .unwrap();
    let late_state = remote
        .get(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    remote
        .put(paykit_lib::PAYKIT_SHARED_STATE_PATH, original)
        .await
        .unwrap();
    let marker = format!(
        "{}{}",
        paykit_lib::PAYKIT_SHARED_STATE_WRITE_PATH_PREFIX,
        blake3::hash(&late_state).to_hex()
    );
    remote.put(&marker, Vec::<u8>::new()).await.unwrap();

    let server_access = session_bootstrap(&testnet, "paykit-server.test")
        .sign_in(&secret, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    let reader = PubkySharedStateStorage::new(TestnetSessionProvider::new(server_access));
    let callback_ran = Arc::new(AtomicBool::new(false));
    let observed_callback = Arc::clone(&callback_ran);
    let started = Instant::now();
    let mut read = tokio::spawn(async move {
        loop {
            let result = reader
                .transaction(|tx| {
                    observed_callback.store(true, Ordering::SeqCst);
                    Ok(tx.load_identity_state().unwrap().initialized_at)
                })
                .await;
            // The lock probe below may acquire the lease before this task starts.
            if result.as_ref().is_err_and(|error| {
                error.is_concurrent_update()
                    || matches!(error, PaykitSdkError::SharedStateBusy { .. })
            }) && started.elapsed() < Duration::from_secs(10)
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            break result;
        }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                !read.is_finished(),
                "the read must wait for the pending write"
            );
            match remote
                .lock(
                    paykit_lib::PAYKIT_SHARED_STATE_PATH,
                    Duration::from_secs(60),
                )
                .await
            {
                Ok(lock) => remote.unlock(&lock).await.unwrap(),
                Err(pubky::Error::Request(pubky::errors::RequestError::Server {
                    status, ..
                })) if status == pubky::StatusCode::LOCKED => break,
                Err(error) => panic!("unexpected lock error: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the independent reader should acquire the shared-state lock");

    // Wait beyond the initial lease to exercise renewal during the real cooldown.
    tokio::time::sleep(Duration::from_secs(70)).await;
    assert!(!read.is_finished());
    assert!(!callback_ran.load(Ordering::SeqCst));
    assert!(remote
        .get(&marker)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .is_empty());
    let competing_callback_ran = AtomicBool::new(false);
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        storage.transaction(|tx| {
            competing_callback_ran.store(true, Ordering::SeqCst);
            Ok(tx.export_storage_state())
        }),
    )
    .await
    .expect("a competing transaction should fail without joining the cooldown")
    .expect_err("the cooldown must retain the exclusive state lock");
    assert!(matches!(error, PaykitSdkError::SharedStateBusy { .. }));
    assert!(!error.is_concurrent_update());
    assert!(!competing_callback_ran.load(Ordering::SeqCst));

    // Admin DAV bypasses the client lock, simulating a previously admitted late publication.
    let admin = testnet.homeserver_app().admin_server().unwrap();
    reqwest::Client::new()
        .put(format!(
            "http://{}/dav/{}{}",
            admin.listen_socket(),
            secret.public_key().to_public_key().unwrap().z32(),
            paykit_lib::PAYKIT_SHARED_STATE_PATH
        ))
        .basic_auth(
            "admin",
            Some(
                pubky_testnet::pubky_homeserver::ConfigToml::default_test_config()
                    .admin
                    .admin_password,
            ),
        )
        .body(late_state)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let reloaded_at = tokio::time::timeout(Duration::from_secs(360), &mut read)
        .await
        .expect("the abandoned write cooldown should finish")
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(300));
    assert!(callback_ran.load(Ordering::SeqCst));
    assert_eq!(reloaded_at, late_initialized_at);
    assert_no_pending_shared_state_writes(&access.session).await;

    tokio::time::timeout(
        Duration::from_secs(10),
        storage.transaction(|tx| {
            let mut identity = tx.load_identity_state().unwrap();
            assert_eq!(identity.initialized_at, late_initialized_at);
            identity.initialized_at += chrono::Duration::seconds(1);
            tx.save_identity_state(identity);
            Ok(())
        }),
    )
    .await
    .expect("normal transactions should not wait once pending markers are removed")
    .unwrap();
    assert_no_pending_shared_state_writes(&access.session).await;
}

#[tokio::test]
async fn test_pubky_shared_state_does_not_write_when_marker_publication_is_rejected() {
    let testnet = build_testnet_with_admin().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = session_bootstrap(&testnet, "bitkit.test")
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
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
    let remote = access.session.storage();
    let original = remote
        .get(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let admin = testnet.homeserver_app().admin_server().unwrap();
    reqwest::Client::new()
        .patch(format!(
            "http://{}/users/{}/quota",
            admin.listen_socket(),
            secret.public_key().to_public_key().unwrap().z32()
        ))
        .header(
            "X-Admin-Password",
            pubky_testnet::pubky_homeserver::ConfigToml::default_test_config()
                .admin
                .admin_password,
        )
        .json(&serde_json::json!({
            "allowed_write_paths": [paykit_lib::PAYKIT_SHARED_STATE_PATH]
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let callback_ran = AtomicBool::new(false);
    let error = storage
        .with_operation(async {
            let error = storage
                .transaction(|tx| {
                    callback_ran.store(true, Ordering::SeqCst);
                    let mut identity = tx.load_identity_state().unwrap();
                    identity.initialized_at += chrono::Duration::seconds(1);
                    tx.save_identity_state(identity);
                    Ok(())
                })
                .await
                .expect_err("a failed marker publication must prevent the state PUT");
            let read_ran = AtomicBool::new(false);
            assert!(storage
                .transaction(|_| {
                    read_ran.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .is_err());
            assert!(!read_ran.load(Ordering::SeqCst));
            Err::<(), _>(error)
        })
        .await
        .expect_err("a failed marker publication must prevent the state PUT");
    assert!(callback_ran.load(Ordering::SeqCst));
    assert!(
        matches!(&error, PaykitSdkError::Transport { context, .. }
            if context == "mark pending Pubky shared-state write"),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        remote
            .get(paykit_lib::PAYKIT_SHARED_STATE_PATH)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        original
    );
    assert_no_pending_shared_state_writes(&access.session).await;
    remote
        .put(paykit_lib::PAYKIT_SHARED_STATE_PATH, original)
        .await
        .expect("the state path itself should still permit writes");
}

async fn assert_no_pending_shared_state_writes(session: &pubky::PubkySession) {
    match session
        .storage()
        .list(paykit_lib::PAYKIT_SHARED_STATE_WRITE_PATH_PREFIX)
        .unwrap()
        .limit(1)
        .send()
        .await
    {
        Ok(markers) => assert!(
            markers.is_empty(),
            "pending write markers remain: {markers:?}"
        ),
        Err(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
            if status == pubky::StatusCode::NOT_FOUND || status == pubky::StatusCode::GONE => {}
        Err(error) => panic!("could not list pending write markers: {error}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pubky_shared_state_rejects_a_competing_writer_until_lock_release() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap(), "paykit-sdk.test")
        .unwrap()
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
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
    let initialized_at = second
        .transaction(|tx| Ok(tx.load_identity_state().unwrap().initialized_at))
        .await
        .unwrap();

    let (pause, loaded_rx, continue_tx) = TransactionPause::new();
    let locked_storage = first.clone();
    let locked_write = std::thread::spawn(move || {
        // The synchronous pause must not stall the shared HTTP connection's driver.
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(locked_storage.transaction(move |tx| {
                pause.wait()?;
                let mut identity = tx.load_identity_state().unwrap();
                identity.initialized_at += chrono::Duration::seconds(2);
                tx.save_identity_state(identity);
                Ok(())
            }))
    });
    loaded_rx
        .recv_timeout(TRANSACTION_PAUSE_TIMEOUT)
        .expect("the writer should hold the shared-state lock");
    let competing_callback_ran = AtomicBool::new(false);
    // Finish or cancel the contender before the paused writer's guard expires.
    // Always resume and join the writer before reporting a contender failure.
    let competing_write = tokio::time::timeout(
        TRANSACTION_PAUSE_TIMEOUT / 2,
        second.transaction(|tx| {
            competing_callback_ran.store(true, Ordering::SeqCst);
            let mut identity = tx.load_identity_state().unwrap();
            identity.initialized_at += chrono::Duration::seconds(1);
            tx.save_identity_state(identity);
            Ok(())
        }),
    )
    .await;
    let resumed = continue_tx.try_send(());
    let locked_result = locked_write
        .join()
        .expect("the paused writer must not panic");
    let competing_write =
        competing_write.expect("lock contention should finish before the writer is resumed");
    resumed.expect("the lock holder should still be waiting");
    locked_result.expect("the lock holder should commit its transaction");

    let error = competing_write.expect_err("a competing writer must not acquire the held lock");
    assert!(error.is_concurrent_update());
    assert!(!competing_callback_ran.load(Ordering::SeqCst));

    let reloaded_at = second
        .transaction(|tx| Ok(tx.load_identity_state().unwrap().initialized_at))
        .await
        .expect("the competing storage instance should read the committed state after unlock");
    assert_eq!(reloaded_at, initialized_at + chrono::Duration::seconds(2));

    second
        .transaction(|tx| {
            let mut identity = tx.load_identity_state().unwrap();
            identity.initialized_at += chrono::Duration::seconds(3);
            tx.save_identity_state(identity);
            Ok(())
        })
        .await
        .expect("the competing writer should succeed after reloading and retrying");
    let final_initialized_at = first
        .transaction(|tx| Ok(tx.load_identity_state().unwrap().initialized_at))
        .await
        .unwrap();
    assert_eq!(
        final_initialized_at,
        initialized_at + chrono::Duration::seconds(5)
    );
}

#[tokio::test]
async fn test_pubky_shared_state_rejects_a_missing_previously_observed_resource() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap(), "paykit-sdk.test")
        .unwrap()
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
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
async fn test_outbound_batches_deliver_with_slow_shared_state() {
    #[derive(Clone)]
    struct TransactionClock {
        started: chrono::DateTime<Utc>,
        elapsed: Arc<AtomicUsize>,
    }

    impl Clock for TransactionClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            self.started + chrono::Duration::seconds(self.elapsed.load(Ordering::SeqCst) as i64)
        }
    }

    struct TimedStorage {
        inner: PubkySharedStateStorage,
        clock: TransactionClock,
    }

    #[async_trait]
    impl StorageAdapter for TimedStorage {
        async fn run_operation_erased<'a>(
            &self,
            operation: StorageOperation<'a>,
        ) -> PaykitResult<Box<dyn Any + Send>> {
            self.inner.run_operation_erased(operation).await
        }

        async fn transaction_erased<'a>(
            &self,
            f: StorageTransactionCallback<'a>,
        ) -> PaykitResult<Box<dyn Any + Send>> {
            self.inner
                .transaction_erased(Box::new(|tx| {
                    self.clock.elapsed.fetch_add(3, Ordering::SeqCst);
                    f(tx)
                }))
                .await
        }
    }

    let pair = linked_homeserver_shared_pair().await;
    let mut additional_peers = Vec::new();
    for _ in 0..3 {
        let peer = TestUser::sign_up(&pair._testnet).await;
        pair.bitkit
            .sdk
            .initiate_link_with_peer(peer.public_key.clone())
            .await
            .unwrap();
        peer.sdk
            .accept_link_with_peer(pair.bitkit.public_key.clone())
            .await
            .unwrap();
        drive_shared_link_to_linked(&pair.bitkit, &peer).await;
        additional_peers.push(peer);
    }

    let mut peers = vec![&pair.bob];
    peers.extend(&additional_peers);
    peers.sort_by(|left, right| left.public_key.as_str().cmp(right.public_key.as_str()));
    let mut expected = Vec::new();
    for peer in &peers {
        let request = pair
            .bitkit
            .sdk
            .propose_payment_request(peer.public_key.clone(), recurring_request_terms())
            .await
            .unwrap();
        expected.push(vec![request.last_outbound_message_id.unwrap()]);
    }

    let clock = TransactionClock {
        started: Utc::now(),
        elapsed: Arc::new(AtomicUsize::new(0)),
    };
    let sdk = PaykitSdk::with_clock(
        TimedStorage {
            inner: pair.bitkit.storage.clone(),
            clock: clock.clone(),
        },
        TestnetSessionProvider::new(pair.bitkit.access.clone()),
        pair.bitkit.adapter.clone(),
        PaykitSdkConfig::new(pair.bitkit.app_id.clone()).unwrap(),
        clock,
    );
    let reports = sdk.process_pending_private_messages().await.unwrap();
    assert_eq!(reports.len(), peers.len());
    for ((report, peer), expected) in reports.into_iter().zip(&peers).zip(expected) {
        assert_eq!(report.counterparty, peer.public_key);
        assert!(report.error.is_none(), "{:?}", report.error);
        let report = report.report.unwrap();
        assert_eq!(report.sent, expected);
        assert!(report.failed.is_empty());
        let received = peer
            .sdk
            .receive_private_messages(pair.bitkit.public_key.clone())
            .await
            .unwrap();
        assert_eq!(received.stream_item_ids.len(), 1);
    }
    let updates = peers
        .iter()
        .map(|peer| PrivatePaymentListReservationUpdate {
            counterparty: peer.public_key.clone(),
            reservations: Vec::new(),
        })
        .collect();
    let cleared = sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(updates, false)
        .await
        .unwrap();
    assert_eq!(cleared.cleared.len(), peers.len());
    assert!(cleared.failed_to_queue.is_empty());
    assert!(cleared.failed_to_deliver.is_empty(), "{cleared:?}");
    for peer in peers {
        let received = peer
            .sdk
            .receive_private_messages(pair.bitkit.public_key.clone())
            .await
            .unwrap();
        assert!(!received.stream_item_ids.is_empty());
        let lists = peer
            .sdk
            .current_private_payment_lists(&pair.bitkit.public_key)
            .await
            .unwrap();
        assert!(lists.iter().any(|list| {
            list.app_id == pair.bitkit.app_id && list.payment_endpoints.is_empty()
        }));
        assert_eq!(
            peer.sdk
                .received_payment_requests_from(&pair.bitkit.public_key)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    assert_no_pending_shared_state_writes(&pair.bitkit.access.session).await;
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
    let locked_send = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(bitkit_send_sdk.process_outbound_private_messages(send_counterparty))
    });
    send_loaded
        .recv_timeout(TRANSACTION_PAUSE_TIMEOUT)
        .expect("the sender should hold the shared-state lock");
    let competing_send = pair
        .server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await;
    continue_send
        .try_send(())
        .expect("the sender holding the lock should resume");
    let successful_send = locked_send
        .join()
        .unwrap()
        .expect("the lock holder should commit both queued messages");
    assert_eq!(successful_send.sent.len(), 2);
    assert!(competing_send
        .expect_err("the competing sender must not acquire the held lock")
        .is_concurrent_update());
    let retried_send = pair
        .server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the competing sender should reload the completed sends after unlock");
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
    assert_eq!(peer_send.sent.len(), 4);

    let (bitkit_receive_sdk, receive_loaded, continue_receive) = pair.bitkit.paused_sdk();
    let receive_counterparty = pair.bob.public_key.clone();
    let locked_receive = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(bitkit_receive_sdk.receive_private_messages(receive_counterparty))
    });
    receive_loaded
        .recv_timeout(TRANSACTION_PAUSE_TIMEOUT)
        .expect("the receiver should hold the shared-state lock");
    let competing_receive = pair
        .server
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await;
    continue_receive
        .try_send(())
        .expect("the receiver holding the lock should resume");
    let successful_receive = locked_receive
        .join()
        .unwrap()
        .expect("the lock holder should commit the confirmations and requests");
    assert_eq!(successful_receive.stream_item_ids.len(), 4);
    assert!(competing_receive
        .expect_err("the competing receiver must not acquire the held lock")
        .is_concurrent_update());
    let retried_receive = pair
        .server
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the competing receiver should reload the committed checkpoint after unlock");
    assert!(retried_receive.stream_item_ids.is_empty());

    let bitkit_state = pair.bitkit.storage_state().await;
    let server_state = pair.server.storage_state().await;
    assert_eq!(bitkit_state, server_state);
    assert!(bitkit_state.peer_link_operation_leases.is_empty());
    assert!(bitkit_state
        .outbound_private_messages
        .iter()
        .all(|message| {
            message.prepared_send.is_none()
                && if message.kind == "paykit.delivery_confirmation" {
                    message.status == OutboundPrivateMessageStatus::Pending
                } else {
                    message.status == OutboundPrivateMessageStatus::Sent
                        && message.confirmed_at.is_some()
                }
        }));
    assert_eq!(
        bitkit_state
            .private_stream_items
            .iter()
            .filter(|item| item.counterparty == pair.bob.public_key)
            .count(),
        4
    );
}

#[tokio::test]
async fn test_private_sends_survive_restart_at_checkpoint_and_publish_boundaries() {
    let pair = linked_homeserver_shared_pair().await;
    for crash_point in [
        PrivateOperationCrashPoint::PreparedStateRejected,
        PrivateOperationCrashPoint::PreparedStateCommitted,
        PrivateOperationCrashPoint::CiphertextPublished,
    ] {
        assert_private_send_survives_restart(&pair, crash_point).await;
        pair.bob
            .sdk
            .process_outbound_private_messages(pair.bitkit.public_key.clone())
            .await
            .unwrap();
        pair.bitkit
            .sdk
            .receive_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap();
    }
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
async fn test_key_rotation_preserves_in_flight_write_and_rejects_old_key() {
    let pair = linked_homeserver_shared_pair().await;
    let (old_sdk, loaded, resume) = pair.server.paused_sdk();
    let contact = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let writing_sdk = old_sdk.clone();
    let writing_contact = contact.clone();
    let locked_write = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(writing_sdk.save_contact(ContactUpdate {
                public_key: writing_contact,
                label: Some("committed before rotation".into()),
            }))
    });
    loaded
        .recv_timeout(TRANSACTION_PAUSE_TIMEOUT)
        .expect("the old-key writer should hold the shared-state lock");

    let replacement = pair
        .secret
        .derive_paykit_identity_secret_key(2)
        .expect("the replacement key should derive");
    let competing_rotation = pair
        .bitkit
        .sdk
        .rotate_paykit_identity_key(replacement.clone())
        .await;
    resume
        .try_send(())
        .expect("the old-key writer should finish before rotation");
    let committed_contact = locked_write
        .join()
        .unwrap()
        .expect("the lock holder should commit before rotation");
    assert!(competing_rotation
        .expect_err("key rotation must not acquire the writer's held lock")
        .is_concurrent_update());

    let registry = pair
        .bitkit
        .sdk
        .rotate_paykit_identity_key(replacement.clone())
        .await
        .expect("key rotation should reload and preserve the completed write");
    assert_eq!(registry.key_generation(), 2);
    let stale_error = old_sdk
        .save_contact(ContactUpdate {
            public_key: contact,
            label: Some("stale writer".into()),
        })
        .await
        .expect_err("the old-key writer must not overwrite rotated state");
    assert!(matches!(stale_error, PaykitSdkError::Identity { .. }));

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
    assert_eq!(
        replacement_sdk.contact_records().await.unwrap(),
        vec![committed_contact]
    );
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

    let terms = recurring_request_terms_builder()
        .required_app_id(Some(pair.bitkit.app_id.clone()))
        .build()
        .unwrap();
    let payment_reference = terms.payment_reference().clone();
    let amount = terms.amount().clone();
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

    let billing_period =
        BillingPeriod::new("2026-08-01T00:00:00Z", "2026-09-01T00:00:00Z").unwrap();
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
        if paykit_lib::get_paykit_noise_key_authorization(
            &access.outbox_client.public_storage(),
            access.session.info().public_key(),
        )
        .await
        .unwrap()
        .is_none()
        {
            sdk.publish_paykit_noise_key_authorization().await.unwrap();
        }
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
    _testnet: TestnetInstance,
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
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
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

async fn wait_for_shared_state_after_crash(
    storage: &PubkySharedStateStorage,
) -> paykit_sdk::storage::StorageState {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            match storage
                .transaction(|tx| Ok(tx.export_storage_state()))
                .await
            {
                Ok(state) => return state,
                Err(error) if error.is_concurrent_update() => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(error) => panic!("shared state should remain readable after crash: {error}"),
            }
        }
    })
    .await
    .expect("the abandoned homeserver lock should expire")
}

async fn assert_private_send_survives_restart(
    pair: &HomeserverSharedPair,
    crash_point: PrivateOperationCrashPoint,
) {
    let initial_link =
        pair.bitkit.storage_state().await.encrypted_link_states[&pair.bob.public_key].clone();
    let rejected = crash_point == PrivateOperationCrashPoint::PreparedStateRejected;
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

    send.abort();
    assert!(send
        .await
        .expect_err("the crashed sender should be aborted")
        .is_cancelled());
    let crashed_state = wait_for_shared_state_after_crash(&pair.bitkit.storage).await;
    let crashed_message = crashed_state
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == outbound_message_id)
        .expect("the outbound record should remain durable");
    assert_eq!(
        crashed_message.status,
        if rejected {
            OutboundPrivateMessageStatus::Pending
        } else {
            OutboundPrivateMessageStatus::Sending
        }
    );
    assert_eq!(crashed_message.prepared_send.is_some(), !rejected);
    assert_eq!(crashed_message.attempt_count, if rejected { 0 } else { 1 });
    assert!(crashed_state
        .peer_link_operation_leases
        .contains_key(&pair.bob.public_key));
    let crashed_link = crashed_state
        .encrypted_link_states
        .get(&pair.bob.public_key)
        .expect("the advanced Encrypted Link snapshot should be durable")
        .clone();
    if rejected {
        assert_eq!(
            crashed_link, initial_link,
            "rejected checkpoint must not advance Noise"
        );
    }

    let received_before_restart = pair
        .bob
        .sdk
        .receive_private_messages(pair.bitkit.public_key.clone())
        .await
        .expect("the peer should inspect the private stream at the crash boundary");
    match crash_point {
        PrivateOperationCrashPoint::PreparedStateRejected
        | PrivateOperationCrashPoint::PreparedStateCommitted => {
            assert!(received_before_restart.stream_item_ids.is_empty());
        }
        PrivateOperationCrashPoint::CiphertextPublished => {
            assert_eq!(received_before_restart.stream_item_ids.len(), 1);
        }
        PrivateOperationCrashPoint::PrivateReceiveCheckpointCommitted => {
            unreachable!("receive crash point is not used by send recovery")
        }
    }

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
        PrivateOperationCrashPoint::PreparedStateRejected
        | PrivateOperationCrashPoint::PreparedStateCommitted => {
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
    assert_eq!(final_message.attempt_count, if rejected { 1 } else { 2 });
    assert!(final_message.prepared_send.is_none());
    assert!(final_state.peer_link_operation_leases.is_empty());
    let final_link = final_state
        .encrypted_link_states
        .get(&pair.bob.public_key)
        .expect("the Encrypted Link state should remain present");
    if rejected {
        assert_eq!(final_link.generation, initial_link.generation + 1);
    } else {
        assert_eq!(
            final_link, &crashed_link,
            "retrying a prepared send must not advance Noise state again"
        );
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

const TRANSACTION_PAUSE_TIMEOUT: Duration = Duration::from_secs(30);

impl TransactionPause {
    fn new() -> (Self, Receiver<()>, SyncSender<()>) {
        let (loaded_tx, loaded_rx) = sync_channel(1);
        let (resume_tx, resume_rx) = sync_channel(1);
        (
            Self {
                loaded: loaded_tx,
                resume: resume_rx,
            },
            loaded_rx,
            resume_tx,
        )
    }

    fn wait(self) -> PaykitResult<()> {
        // An abandoned test must roll back and unlock, not panic inside the transaction.
        self.loaded
            .try_send(())
            .map_err(|error| PaykitSdkError::Storage {
                context: "shared-state transaction pause observer is unavailable".into(),
                source: Some(error.into()),
            })?;
        self.resume
            .recv_timeout(TRANSACTION_PAUSE_TIMEOUT)
            .map_err(|error| PaykitSdkError::Storage {
                context: "shared-state transaction was not resumed".into(),
                source: Some(error.into()),
            })
    }
}

impl OneShotPausedStorage {
    fn new(inner: PubkySharedStateStorage) -> (Self, Receiver<()>, SyncSender<()>) {
        let (pause, loaded_rx, resume_tx) = TransactionPause::new();
        (
            Self {
                inner,
                pause: Arc::new(Mutex::new(Some(pause))),
            },
            loaded_rx,
            resume_tx,
        )
    }
}

#[async_trait]
impl StorageAdapter for OneShotPausedStorage {
    async fn run_operation_erased<'a>(
        &self,
        operation: StorageOperation<'a>,
    ) -> PaykitResult<Box<dyn Any + Send>> {
        self.inner.run_operation_erased(operation).await
    }

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
                        // The callback still holds the file lock; resuming allows commit and unlock.
                        pause.wait()?;
                    }
                }
                result
            }))
            .await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateOperationCrashPoint {
    PreparedStateRejected,
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
    async fn run_operation_erased<'a>(
        &self,
        operation: StorageOperation<'a>,
    ) -> PaykitResult<Box<dyn Any + Send>> {
        self.inner.run_operation_erased(operation).await
    }

    async fn transaction_erased<'a>(
        &self,
        transaction: StorageTransactionCallback<'a>,
    ) -> PaykitResult<Box<dyn Any + Send>> {
        let prepared_in_transaction = Arc::new(AtomicBool::new(false));
        let observed_prepared = Arc::clone(&prepared_in_transaction);
        let receive_in_transaction = Arc::new(AtomicBool::new(false));
        let observed_receive = Arc::clone(&receive_in_transaction);
        let sent_in_transaction = Arc::new(AtomicBool::new(false));
        let observed_sent = Arc::clone(&sent_in_transaction);
        let crash_point = self.crash_point;
        let result = self
            .inner
            .transaction_erased(Box::new(move |tx| {
                let before = tx.export_storage_state();
                let result = transaction(tx);
                let after = tx.export_storage_state();
                if result.is_ok() && contains_new_prepared_send(&before, &after) {
                    observed_prepared.store(true, Ordering::SeqCst);
                    if crash_point == PrivateOperationCrashPoint::PreparedStateRejected {
                        return Err(PaykitSdkError::Storage {
                            context: "injected rejection of prepared send checkpoint".into(),
                            source: None,
                        });
                    }
                }
                if result.is_ok() && contains_new_private_stream_item(&before, &after) {
                    observed_receive.store(true, Ordering::SeqCst);
                }
                if result.is_ok()
                    && crash_point == PrivateOperationCrashPoint::CiphertextPublished
                    && after.outbound_private_messages.iter().any(|message| {
                        message.status == OutboundPrivateMessageStatus::Sent
                            && before.outbound_private_messages.iter().any(|previous| {
                                previous.outbound_message_id == message.outbound_message_id
                                    && previous.status == OutboundPrivateMessageStatus::Sending
                                    && previous.prepared_send.is_some()
                            })
                    })
                {
                    observed_sent.store(true, Ordering::SeqCst);
                    // Keep the prepared send durable without committing its acknowledgement.
                    return Err(PaykitSdkError::Storage {
                        context: "injected interruption after ciphertext publication".into(),
                        source: None,
                    });
                }
                result
            }))
            .await;

        if sent_in_transaction.load(Ordering::SeqCst) {
            self.stop_at_crash_boundary().await;
        }
        if prepared_in_transaction.load(Ordering::SeqCst)
            && self.crash_point == PrivateOperationCrashPoint::PreparedStateRejected
        {
            self.stop_at_crash_boundary().await;
        }
        let result = result?;
        if prepared_in_transaction.load(Ordering::SeqCst)
            && self.crash_point == PrivateOperationCrashPoint::PreparedStateCommitted
        {
            self.stop_at_crash_boundary().await;
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
        .additional_app(&pair._testnet, app_id("paykit-server"), "Paykit Server")
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

    let signed_out = pair
        .alice
        .sdk
        .sign_out()
        .await
        .expect("signing out one application should succeed");
    assert_eq!(signed_out.capability, PubkyIdentityCapability::SignedOut);

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
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .expect("the payee should confirm receipt of the acceptance and cancellation");
    alice_server
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the removing application should consume the confirmations");

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
        .additional_app(&pair._testnet, app_id("paykit-server"), "Paykit Server")
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

#[tokio::test]
async fn test_app_removal_cleans_malformed_endpoint_payloads() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    let prefix = format!(
        "{}apps/{}/endpoints/",
        paykit_lib::PAYKIT_PATH_PREFIX,
        user.app_id
    );
    for (identifier, bytes) in [
        ("invalid-utf8", vec![0xff]),
        (
            "oversized",
            vec![b'x'; paykit_lib::PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES + 1],
        ),
    ] {
        user.access
            .session
            .storage()
            .put(format!("{prefix}{identifier}"), bytes)
            .await
            .unwrap();
    }
    let registry = user.sdk.remove_paykit_app().await.unwrap();
    assert!(!registry.apps().contains_key(&user.app_id));
    assert!(paykit_lib::list_payment_endpoint_identifiers(
        &user.access.outbox_client.public_storage(),
        &user.public_key.to_public_key().unwrap(),
        &user.app_id,
    )
    .await
    .unwrap()
    .is_empty());
}

fn recurring_request_terms() -> PaymentRequestTerms {
    recurring_request_terms_builder().build().unwrap()
}

fn recurring_request_terms_builder() -> PaymentRequestTermsBuilder {
    let starts_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let recurrence = Recurrence::try_from(RecurrenceConfig {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: starts_at.clone(),
        anchor: starts_at,
        ends_at: None,
    })
    .unwrap();
    PaymentRequestTerms::builder(
        PaymentAmount::new("0.001", "btc").unwrap(),
        PaymentReference::new("shared-identity-subscription").unwrap(),
        vec![PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()],
    )
    .recurrence(Some(recurrence))
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
