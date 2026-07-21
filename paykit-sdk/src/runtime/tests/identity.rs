use super::*;

#[tokio::test]
async fn test_initialize_persists_signed_out_identity() {
    let storage = InMemoryStorage::new();
    let pubky = TestPubkySessionProvider { session: None };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        pubky,
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let report = sdk.initialize().await.unwrap();

    assert!(!report.identity.live_session_available);
    let stored = storage.snapshot().unwrap().identity_state.unwrap();
    assert!(stored.local_pubky_public_key.is_none());
    assert!(stored.local_receiver_noise_public_key.is_none());
    assert_eq!(stored.initialized_at, FixedClock.now());
}

#[tokio::test]
async fn test_initialize_without_live_session_preserves_identity_scoped_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    local_pubky_public_key: Some(local_public_key),
                    local_receiver_noise_public_key: Some(receiver_noise_public_key()),
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 3,
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
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
                    identifier: "btc-lightning-bolt11".into(),
                    payload: Some("payload".into()),
                    status: PublicationStatus::Published,
                    updated_at: FixedClock.now(),
                    last_error: None,
                });
                tx.save_contact_record(ContactRecord {
                    public_key: counterparty.clone(),
                    receiver_paths: vec![receiver_path()],
                    label: Some("Alice".into()),
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::NotPublished,
                    public_contact_marker_receiver_path: None,
                    public_contact_published_at: None,
                    public_contact_removed_at: None,
                    public_contact_last_error: None,
                });
                tx.insert_outbound_private_message(crate::storage::NewOutboundPrivateMessage::new(
                    counterparty,
                    receiver_path(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let report = sdk.initialize().await.unwrap();

    let snapshot = storage.snapshot().unwrap();
    let identity = snapshot.identity_state.unwrap();
    assert!(!report.identity.live_session_available);
    assert_eq!(
        identity.local_pubky_public_key.as_ref(),
        Some(&local_public_key)
    );
    assert_eq!(identity.sign_out_generation, 3);
    assert_eq!(snapshot.linked_peers.len(), 1);
    assert_eq!(snapshot.contact_records.len(), 1);
    assert_eq!(snapshot.public_endpoint_records.len(), 1);
    assert_eq!(snapshot.outbound_private_messages.len(), 1);
}

#[tokio::test]
async fn test_receiver_noise_key_change_clears_only_private_identity_scoped_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let next_receiver_noise_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::from_secret(&[8; 32]).public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    local_pubky_public_key: Some(local_public_key),
                    local_receiver_noise_public_key: Some(receiver_noise_public_key()),
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 3,
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
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
                    identifier: "btc-lightning-bolt11".into(),
                    payload: Some("payload".into()),
                    status: PublicationStatus::Published,
                    updated_at: FixedClock.now(),
                    last_error: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();

    let active_identity = ActiveReceiverIdentity {
        local_pubky_public_key: local_public_key,
        local_receiver_noise_public_key: next_receiver_noise_public_key.clone(),
    };
    storage
        .transaction(move |tx| {
            Ok(refresh_active_identity(
                tx,
                active_identity,
                FixedClock.now(),
            ))
        })
        .await
        .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let identity = snapshot.identity_state.unwrap();
    assert_eq!(
        identity.local_receiver_noise_public_key,
        Some(next_receiver_noise_public_key)
    );
    assert_eq!(identity.sign_out_generation, 3);
    assert!(snapshot.linked_peers.is_empty());
    assert_eq!(snapshot.public_endpoint_records.len(), 1);
}

#[tokio::test]
async fn test_identity_status_cached_identity_requires_live_session() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            local_pubky_public_key: Some(local_public_key.clone()),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let status = sdk.identity_status().await.unwrap().unwrap();

    assert_eq!(status.public_key, Some(local_public_key));
    assert!(!status.live_session_available);
}

#[tokio::test]
async fn test_sign_out_clears_identity_scoped_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    local_pubky_public_key: Some(local_public_key),
                    local_receiver_noise_public_key: Some(receiver_noise_public_key()),
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 3,
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
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
                    identifier: "btc-lightning-bolt11".into(),
                    payload: Some("payload".into()),
                    status: PublicationStatus::Published,
                    updated_at: FixedClock.now(),
                    last_error: None,
                });
                tx.save_contact_record(ContactRecord {
                    public_key: counterparty.clone(),
                    receiver_paths: vec![receiver_path()],
                    label: Some("Alice".into()),
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::NotPublished,
                    public_contact_marker_receiver_path: None,
                    public_contact_published_at: None,
                    public_contact_removed_at: None,
                    public_contact_last_error: None,
                });
                tx.insert_outbound_private_message(crate::storage::NewOutboundPrivateMessage::new(
                    counterparty,
                    receiver_path(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let status = sdk.sign_out().await.unwrap();

    assert!(status.public_key.is_none());
    assert!(!status.live_session_available);
    let snapshot = storage.snapshot().unwrap();
    let identity = snapshot.identity_state.unwrap();
    assert_eq!(identity.sign_out_generation, 4);
    assert!(identity.local_pubky_public_key.is_none());
    assert!(identity.local_receiver_noise_public_key.is_none());
    assert!(snapshot.linked_peers.is_empty());
    assert!(snapshot.contact_records.is_empty());
    assert!(snapshot.public_endpoint_records.is_empty());
    assert!(snapshot.outbound_private_messages.is_empty());
}

#[tokio::test]
async fn test_sign_out_rejects_concurrent_identity_operation() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let _guard = sdk.claim_identity_operation("test operation").unwrap();

    let result = sdk.sign_out().await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_sign_out_provider_failure_preserves_identity_scoped_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    local_pubky_public_key: Some(local_public_key),
                    local_receiver_noise_public_key: Some(receiver_noise_public_key()),
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 3,
                });
                tx.save_contact_record(ContactRecord {
                    public_key: counterparty,
                    receiver_paths: vec![receiver_path()],
                    label: Some("Alice".into()),
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::NotPublished,
                    public_contact_marker_receiver_path: None,
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
        FailingClearSessionProvider,
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.sign_out().await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let snapshot = storage.snapshot().unwrap();
    let identity = snapshot.identity_state.unwrap();
    assert_eq!(identity.sign_out_generation, 3);
    assert!(identity.local_pubky_public_key.is_some());
    assert_eq!(snapshot.contact_records.len(), 1);
}
