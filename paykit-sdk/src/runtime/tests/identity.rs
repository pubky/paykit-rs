use super::*;

#[tokio::test]
async fn test_initialize_persists_signed_out_identity() {
    let storage = InMemoryStorage::new();
    let pubky = TestPubkySessionProvider { session: None };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        pubky,
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let report = sdk.initialize().await.unwrap();

    assert_eq!(report.capability, PubkyIdentityCapability::SignedOut);
    let stored = storage.snapshot().unwrap().identity_state.unwrap();
    assert!(stored.public_key.is_none());
    assert_eq!(stored.initialized_at, FixedClock.now());
}

#[tokio::test]
async fn test_initialize_without_live_session_preserves_identity_scoped_state() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(counterparty.clone()),
                    initialized_at: FixedClock.now(),
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_public_endpoint_record(PublicEndpointRecord {
                    app_id: app_id(),
                    identifier: "btc-lightning-bolt11".into(),
                    payload: Some("payload".into()),
                    status: PublicationStatus::Published,
                    updated_at: FixedClock.now(),
                    last_error: None,
                });
                tx.save_contact_record(ContactRecord {
                    public_key: counterparty.clone(),
                    label: Some("Alice".into()),
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::NotPublished,
                    public_contact_published_at: None,
                    public_contact_removed_at: None,
                    public_contact_last_error: None,
                });
                tx.insert_outbound_private_message(
                    crate::storage::NewOutboundPrivateMessage::new(
                        counterparty,
                        app_id(),
                        "paykit.private_payment_list".into(),
                        private_list_json(),
                        FixedClock.now(),
                    ),
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let report = sdk.initialize().await.unwrap();

    let snapshot = storage.snapshot().unwrap();
    let identity = snapshot.identity_state.unwrap();
    assert_eq!(report.capability, PubkyIdentityCapability::SignedOut);
    assert_eq!(identity.public_key.as_ref(), Some(&counterparty));
    assert_eq!(snapshot.linked_peers.len(), 1);
    assert_eq!(snapshot.contact_records.len(), 1);
    assert_eq!(snapshot.public_endpoint_records.len(), 1);
    assert_eq!(snapshot.outbound_private_messages.len(), 1);
}

#[tokio::test]
async fn test_identity_mismatch_preserves_existing_state() {
    let storage = InMemoryStorage::new();
    let stored_identity = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let active_identity = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let stored_identity = stored_identity.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(stored_identity),
                    initialized_at: FixedClock.now(),
                });
                tx.save_contact_record(ContactRecord {
                    public_key: contact,
                    label: Some("Alice".into()),
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::NotPublished,
                    public_contact_published_at: None,
                    public_contact_removed_at: None,
                    public_contact_last_error: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();

    let result = storage
        .transaction(move |tx| bind_storage_to_identity(tx, active_identity, FixedClock.now()))
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(
        snapshot.identity_state.unwrap().public_key,
        Some(stored_identity)
    );
    assert_eq!(snapshot.contact_records.len(), 1);
}

#[tokio::test]
async fn test_binding_same_identity_preserves_initialization_time() {
    let storage = InMemoryStorage::new();
    let identity = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let initial_time = FixedClock.now();
    storage
        .save_identity_state(IdentityState {
            public_key: Some(identity.clone()),
            initialized_at: initial_time,
        })
        .await
        .unwrap();

    let bound = storage
        .transaction(move |tx| {
            bind_storage_to_identity(tx, identity, initial_time + chrono::Duration::hours(1))
        })
        .await
        .unwrap();

    assert_eq!(bound.initialized_at, initial_time);
    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .identity_state
            .unwrap()
            .initialized_at,
        initial_time
    );
}

#[tokio::test]
async fn test_identity_status_cached_capability_requires_live_session() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let status = sdk.identity_status().await.unwrap().unwrap();

    assert_eq!(status.capability, PubkyIdentityCapability::SignedOut);
}

#[tokio::test]
async fn test_forget_session_access_preserves_identity_scoped_state() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(counterparty.clone()),
                    initialized_at: FixedClock.now(),
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_public_endpoint_record(PublicEndpointRecord {
                    app_id: app_id(),
                    identifier: "btc-lightning-bolt11".into(),
                    payload: Some("payload".into()),
                    status: PublicationStatus::Published,
                    updated_at: FixedClock.now(),
                    last_error: None,
                });
                tx.save_contact_record(ContactRecord {
                    public_key: counterparty.clone(),
                    label: Some("Alice".into()),
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::NotPublished,
                    public_contact_published_at: None,
                    public_contact_removed_at: None,
                    public_contact_last_error: None,
                });
                tx.insert_outbound_private_message(
                    crate::storage::NewOutboundPrivateMessage::new(
                        counterparty,
                        app_id(),
                        "paykit.private_payment_list".into(),
                        private_list_json(),
                        FixedClock.now(),
                    ),
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let status = sdk.forget_session_access().await.unwrap();

    assert_eq!(status.capability, PubkyIdentityCapability::SignedOut);
    let snapshot = storage.snapshot().unwrap();
    let identity = snapshot.identity_state.unwrap();
    assert_eq!(identity.public_key.as_ref(), Some(&counterparty));
    assert_eq!(snapshot.linked_peers.len(), 1);
    assert_eq!(snapshot.contact_records.len(), 1);
    assert_eq!(snapshot.public_endpoint_records.len(), 1);
    assert_eq!(snapshot.outbound_private_messages.len(), 1);
}

#[tokio::test]
async fn test_sign_out_without_initialized_state_does_not_create_state() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let status = sdk.sign_out().await.unwrap();

    assert_eq!(status.capability, PubkyIdentityCapability::SignedOut);
    assert!(storage.snapshot().unwrap().identity_state.is_none());
}

#[tokio::test]
async fn test_sign_out_waits_for_live_session_operations() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    let cleared = Arc::new(AtomicBool::new(false));
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        RecordingClearSessionProvider {
            cleared: Arc::clone(&cleared),
        },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let session_operation = Arc::clone(&sdk.session_operation_gate).read_owned().await;
    let mut sign_out = Box::pin(sdk.sign_out());
    let mut context = Context::from_waker(Waker::noop());

    assert!(matches!(
        sign_out.as_mut().poll(&mut context),
        Poll::Pending
    ));
    assert!(!cleared.load(Ordering::SeqCst));

    drop(session_operation);
    sign_out.await.unwrap();
    assert!(cleared.load(Ordering::SeqCst));
}

#[tokio::test]
async fn test_identity_status_holds_session_gate_until_provider_load_finishes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Barrier;

    struct BlockingSessionProvider {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
        block_next_load: Arc<AtomicBool>,
        cleared: Arc<AtomicBool>,
    }

    #[async_trait]
    impl PubkySessionProvider for BlockingSessionProvider {
        async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
            if self.block_next_load.swap(false, Ordering::SeqCst) {
                self.entered.wait().await;
                self.release.wait().await;
            }
            Ok(None)
        }

        async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
            Ok(None)
        }

        async fn clear_session_access(&self) -> Result<()> {
            self.cleared.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let block_next_load = Arc::new(AtomicBool::new(true));
    let cleared = Arc::new(AtomicBool::new(false));
    let sdk = Arc::new(PaykitSdk::with_clock(
        InMemoryStorage::new(),
        BlockingSessionProvider {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            block_next_load,
            cleared: Arc::clone(&cleared),
        },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    ));
    let status_sdk = Arc::clone(&sdk);
    let status = tokio::spawn(async move { status_sdk.identity_status().await });
    entered.wait().await;
    let sign_out_sdk = Arc::clone(&sdk);
    let sign_out = tokio::spawn(async move { sign_out_sdk.sign_out().await });
    tokio::task::yield_now().await;

    assert!(!cleared.load(Ordering::SeqCst));

    release.wait().await;
    status.await.unwrap().unwrap();
    sign_out.await.unwrap().unwrap();
    assert!(cleared.load(Ordering::SeqCst));
}

#[tokio::test]
async fn test_sign_out_rejects_concurrent_identity_operation() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let _guard = sdk.claim_identity_operation("test operation").unwrap();

    let result = sdk.sign_out().await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_sign_out_without_live_session_preserves_identity_scoped_state() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(counterparty.clone()),
                    initialized_at: FixedClock.now(),
                });
                tx.save_contact_record(ContactRecord {
                    public_key: counterparty,
                    label: Some("Alice".into()),
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::NotPublished,
                    public_contact_published_at: None,
                    public_contact_removed_at: None,
                    public_contact_last_error: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.sign_out().await;

    assert!(matches!(
        result,
        Err(PaykitSdkError::Identity { context, .. })
            if context.contains("without live session access")
    ));
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.identity_state.is_some());
    assert_eq!(snapshot.contact_records.len(), 1);
}

#[tokio::test]
async fn test_sign_out_preserves_session_when_storage_is_unavailable() {
    #[derive(Clone)]
    struct FailingStorage;

    #[async_trait]
    impl StorageAdapter for FailingStorage {
        async fn transaction_erased<'a>(
            &self,
            _f: crate::storage::StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn std::any::Any + Send>> {
            Err(PaykitSdkError::Storage {
                context: "shared state is unavailable".into(),
                source: None,
            })
        }
    }

    let cleared = Arc::new(AtomicBool::new(false));
    let sdk = PaykitSdk::with_clock(
        FailingStorage,
        RecordingClearSessionProvider {
            cleared: Arc::clone(&cleared),
        },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.sign_out().await;

    assert!(matches!(result, Err(PaykitSdkError::Storage { .. })));
    assert!(!cleared.load(Ordering::SeqCst));
}
