use super::*;

#[tokio::test]
async fn test_contact_records_save_list_and_remove_locally() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
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

    let saved = sdk
        .save_contact(ContactUpdate {
            public_key: contact_public_key.clone(),
            receiver_paths: vec![receiver_path()],
            label: Some("Alice".into()),
        })
        .await
        .unwrap();

    assert_eq!(saved.label.as_deref(), Some("Alice"));
    assert_eq!(sdk.contact_records().await.unwrap(), vec![saved.clone()]);
    assert_eq!(
        sdk.contact_record(&contact_public_key).await.unwrap(),
        Some(saved.clone())
    );
    assert_eq!(
        sdk.remove_contact(&contact_public_key).await.unwrap(),
        Some(saved)
    );
    assert!(sdk
        .contact_record(&contact_public_key)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_save_contact_empty_label_clears_existing_label() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    sdk.save_contact(ContactUpdate {
        public_key: contact_public_key.clone(),
        receiver_paths: vec![receiver_path()],
        label: Some("Alice".into()),
    })
    .await
    .unwrap();
    let updated = sdk
        .save_contact(ContactUpdate {
            public_key: contact_public_key,
            receiver_paths: vec![receiver_path()],
            label: Some(String::new()),
        })
        .await
        .unwrap();

    assert!(updated.label.is_none());
}

#[tokio::test]
async fn test_publish_paykit_blob_requires_session() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let result = sdk
        .publish_paykit_blob("avatar.jpg".into(), vec![1, 2, 3])
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_delete_paykit_profile_requires_session() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.delete_paykit_profile().await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_upload_profile_avatar_rejects_unsupported_content_type() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.upload_profile_avatar(vec![1, 2, 3], "text/plain").await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_delete_paykit_blob_requires_initialized_session() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .delete_paykit_blob("/pub/paykit/v0/paykit/wallet/blobs/avatar.jpg")
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_fetch_pubky_file_requires_public_storage() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .fetch_pubky_file("pubky://invalid/pub/paykit/v0/paykit/wallet/blobs/avatar.jpg")
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_remove_contact_blocks_when_public_marker_may_exist() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    receiver_paths: vec![receiver_path()],
                    label: None,
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::Published,
                    public_contact_marker_receiver_path: Some(receiver_path()),
                    public_contact_published_at: Some(FixedClock.now()),
                    public_contact_removed_at: None,
                    public_contact_last_error: None,
                });
                Ok(())
            }
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

    let result = sdk.remove_contact(&contact_public_key).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
}

#[tokio::test]
async fn test_publish_public_contact_does_not_mark_pending_without_session() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    receiver_paths: vec![receiver_path()],
                    label: None,
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
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig {
            public_contact_sharing: PublicContactSharingPolicy::ConfiguredPublicNamespace,
            ..PaykitSdkConfig::default()
        },
        FixedClock,
    );

    let result = sdk
        .publish_public_contact(contact_public_key.clone(), receiver_path())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let record = storage
        .snapshot()
        .unwrap()
        .contact_records
        .get(&contact_public_key)
        .unwrap()
        .clone();
    assert_eq!(
        record.public_contact_marker_status,
        crate::PublicationStatus::NotPublished
    );
}

#[tokio::test]
async fn test_remove_public_contact_cleanup_is_allowed_when_sharing_disabled() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    receiver_paths: vec![receiver_path()],
                    label: None,
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::Published,
                    public_contact_marker_receiver_path: Some(receiver_path()),
                    public_contact_published_at: Some(FixedClock.now()),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .remove_public_contact(contact_public_key.clone(), receiver_path())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let record = storage
        .snapshot()
        .unwrap()
        .contact_records
        .get(&contact_public_key)
        .unwrap()
        .clone();
    assert_eq!(
        record.public_contact_marker_status,
        crate::PublicationStatus::Published
    );
}

#[tokio::test]
async fn test_remove_public_contact_without_local_record_still_requires_session() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let result = sdk
        .remove_public_contact(contact_public_key, receiver_path())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_sync_public_contact_markers_returns_empty_without_pending_markers() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let records = sdk.sync_public_contact_markers().await.unwrap();

    assert!(records.is_empty());
}

#[tokio::test]
async fn test_sync_public_contact_markers_preserves_pending_without_session() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    receiver_paths: vec![receiver_path()],
                    label: None,
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::PendingPublication,
                    public_contact_marker_receiver_path: Some(receiver_path()),
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
        PaykitSdkConfig {
            public_contact_sharing: PublicContactSharingPolicy::ConfiguredPublicNamespace,
            ..PaykitSdkConfig::default()
        },
        FixedClock,
    );

    let result = sdk.sync_public_contact_markers().await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let record = storage
        .snapshot()
        .unwrap()
        .contact_records
        .get(&contact_public_key)
        .unwrap()
        .clone();
    assert_eq!(
        record.public_contact_marker_status,
        crate::PublicationStatus::PendingPublication
    );
}

#[tokio::test]
async fn test_sync_public_contact_markers_fails_pending_publication_when_sharing_disabled() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    receiver_paths: vec![receiver_path()],
                    label: None,
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::PendingPublication,
                    public_contact_marker_receiver_path: Some(receiver_path()),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let records = sdk.sync_public_contact_markers().await.unwrap();

    assert!(records.is_empty());
    let record = storage
        .snapshot()
        .unwrap()
        .contact_records
        .get(&contact_public_key)
        .unwrap()
        .clone();
    assert_eq!(
        record.public_contact_marker_status,
        crate::PublicationStatus::Failed
    );
    assert_eq!(
        record.public_contact_last_error.as_deref(),
        Some("public contact sharing is disabled")
    );
}

#[tokio::test]
async fn test_save_contact_requires_initialized_identity() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());

    let result = sdk
        .save_contact(ContactUpdate {
            public_key: contact_public_key,
            receiver_paths: vec![receiver_path()],
            label: None,
        })
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}
