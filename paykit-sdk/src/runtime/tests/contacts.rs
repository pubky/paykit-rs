use super::*;
use std::sync::atomic::AtomicUsize;

struct ContactTestStorage {
    inner: InMemoryStorage,
    transactions: Arc<AtomicUsize>,
}

#[async_trait]
impl StorageAdapter for ContactTestStorage {
    async fn transaction_erased<'a>(
        &self,
        callback: crate::storage::StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        self.transactions.fetch_add(1, Ordering::SeqCst);
        self.inner
            .transaction_erased(Box::new(move |tx| {
                let before = tx.export_storage_state();
                let result = callback(tx);
                // Inspect the draft before rollback to catch validation after mutation.
                if result.is_err() {
                    assert_eq!(tx.export_storage_state(), before);
                }
                result
            }))
            .await
    }
}

fn public_resource(path: &str) -> pubky::PubkyResource {
    let owner = pubky::Keypair::random().public_key();
    format!("pubky://{}{path}", owner.z32()).parse().unwrap()
}

#[test]
fn test_public_resource_cursor_must_advance() {
    let page = vec![public_resource("/pub/pubky.app/follows/alice")];
    let cursor = next_public_resource_cursor(0, &page, None, 10, "fetch follows").unwrap();

    let error =
        next_public_resource_cursor(1, &page, Some(&cursor), 10, "fetch follows").unwrap_err();

    assert!(matches!(error, PaykitSdkError::Protocol { .. }));
}

#[test]
fn test_public_resource_page_respects_entry_limit() {
    let page = vec![
        public_resource("/pub/pubky.app/follows/alice"),
        public_resource("/pub/pubky.app/follows/bob"),
    ];

    let error = next_public_resource_cursor(0, &page, None, 1, "fetch follows").unwrap_err();

    assert!(matches!(error, PaykitSdkError::Protocol { .. }));
    assert!(require_public_resource_entry_limit(0, "fetch follows").is_err());
}

#[test]
fn test_public_response_size_is_bounded_with_and_without_content_length() {
    assert!(require_positive_response_limit(0, "fetch profile").is_err());
    assert!(require_response_size_within_limit(Some(0), 0, "fetch profile").is_ok());
    assert!(require_response_size_within_limit(Some(5), 4, "fetch profile").is_err());
    assert!(require_response_size_within_limit(None, 4, "fetch profile").is_ok());

    let mut bytes = vec![1, 2];
    assert!(append_response_chunk(&mut bytes, &[3, 4], 4, "fetch profile").is_ok());
    assert!(append_response_chunk(&mut bytes, &[5], 4, "fetch profile").is_err());
}

#[tokio::test]
async fn test_contact_records_save_list_and_remove_locally() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            initialized_at: FixedClock.now(),
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

    let saved = sdk
        .save_contact(ContactUpdate {
            public_key: contact_public_key.clone(),
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

    sdk.save_contact(ContactUpdate {
        public_key: contact_public_key.clone(),
        label: Some("Alice".into()),
    })
    .await
    .unwrap();
    let updated = sdk
        .save_contact(ContactUpdate {
            public_key: contact_public_key,
            label: Some(String::new()),
        })
        .await
        .unwrap();

    assert!(updated.label.is_none());
}

#[tokio::test]
async fn test_save_contacts_preserves_input_order_and_existing_metadata() {
    let local = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let first = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let second = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let earlier = FixedClock.now() - chrono::Duration::days(1);
    let existing = ContactRecord::from_update(
        ContactUpdate {
            public_key: first.clone(),
            label: Some("Previous label".into()),
        },
        None,
        earlier,
    )
    .with_profile(
        Some(PaykitProfile {
            display_name: Some("Cached profile".into()),
            image_uri: None,
            extra: None,
        }),
        earlier,
    )
    .mark_public_contact_published(earlier)
    .mark_public_contact_removal_pending(earlier)
    .mark_public_contact_failed("Retry marker removal".into(), earlier);
    let inner = InMemoryStorage::from_state(crate::storage::StorageState {
        identity_state: Some(IdentityState {
            public_key: Some(local),
            initialized_at: earlier,
        }),
        contact_records: HashMap::from([(first.clone(), existing.clone())]),
        ..Default::default()
    });
    let transactions = Arc::new(AtomicUsize::new(0));
    let sdk = PaykitSdk::with_clock(
        ContactTestStorage {
            inner: inner.clone(),
            transactions: transactions.clone(),
        },
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let records = sdk
        .save_contacts(vec![
            ContactUpdate {
                public_key: first.clone(),
                label: Some(" First update ".into()),
            },
            ContactUpdate {
                public_key: second.clone(),
                label: None,
            },
            ContactUpdate {
                public_key: first.clone(),
                label: Some("   ".into()),
            },
        ])
        .await
        .unwrap();

    assert_eq!(transactions.load(Ordering::SeqCst), 1);
    assert_eq!(records.len(), 3);
    let mut expected = existing;
    expected.label = Some("First update".into());
    expected.updated_at = FixedClock.now();
    assert_eq!(records[0], expected);
    expected.label = None;
    assert_eq!(records[2], expected);
    assert_eq!(records[1].public_key, second);
    assert!(records[1].label.is_none());
    assert!(records[1].profile.is_none());
    assert_eq!(records[1].created_at, FixedClock.now());
    assert_eq!(records[1].updated_at, FixedClock.now());
    assert_eq!(
        records[1].public_contact_marker_status,
        PublicationStatus::NotPublished
    );
    assert_eq!(
        inner.snapshot().unwrap().contact_records,
        HashMap::from([(first, records[2].clone()), (second, records[1].clone())])
    );
}

#[tokio::test]
async fn test_save_contacts_61_records_use_one_transaction() {
    let inner = InMemoryStorage::from_state(crate::storage::StorageState {
        identity_state: Some(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
        }),
        ..Default::default()
    });
    let transactions = Arc::new(AtomicUsize::new(0));
    let sdk = PaykitSdk::with_clock(
        ContactTestStorage {
            inner: inner.clone(),
            transactions: transactions.clone(),
        },
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let updates: Vec<_> = (0..61)
        .map(|_| ContactUpdate {
            public_key: PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
            label: None,
        })
        .collect();
    let expected_keys: Vec<_> = updates
        .iter()
        .map(|update| update.public_key.clone())
        .collect();

    let records = sdk.save_contacts(updates).await.unwrap();

    assert_eq!(transactions.load(Ordering::SeqCst), 1);
    assert_eq!(inner.snapshot().unwrap().contact_records.len(), 61);
    assert_eq!(
        records
            .into_iter()
            .map(|record| record.public_key)
            .collect::<Vec<_>>(),
        expected_keys
    );
}

#[tokio::test]
async fn test_save_contacts_rejects_invalid_batch_before_mutation() {
    for case in ["invalid_label", "self_contact", "uninitialized"] {
        let local = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let first = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let second = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let original = ContactRecord::from_update(
            ContactUpdate {
                public_key: first.clone(),
                label: Some("Original".into()),
            },
            None,
            FixedClock.now(),
        );
        let state = crate::storage::StorageState {
            identity_state: (case != "uninitialized").then(|| IdentityState {
                public_key: Some(local.clone()),
                initialized_at: FixedClock.now(),
            }),
            contact_records: HashMap::from([(first.clone(), original)]),
            ..Default::default()
        };
        let inner = InMemoryStorage::from_state(state.clone());
        let transactions = Arc::new(AtomicUsize::new(0));
        let sdk = PaykitSdk::with_clock(
            ContactTestStorage {
                inner: inner.clone(),
                transactions: transactions.clone(),
            },
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::new("test-app").unwrap(),
            FixedClock,
        );
        let error = sdk
            .save_contacts(vec![
                ContactUpdate {
                    public_key: first,
                    label: Some("Changed".into()),
                },
                ContactUpdate {
                    public_key: if case == "self_contact" {
                        local
                    } else {
                        second
                    },
                    label: (case == "invalid_label").then(|| "x".repeat(129)),
                },
            ])
            .await
            .unwrap_err();

        assert!(matches!(
            (case, error),
            ("invalid_label", PaykitSdkError::Protocol { .. })
                | ("self_contact", PaykitSdkError::Policy { .. })
                | ("uninitialized", PaykitSdkError::Identity { .. })
        ));
        assert_eq!(inner.snapshot().unwrap(), state);
        assert_eq!(
            transactions.load(Ordering::SeqCst),
            usize::from(case != "invalid_label")
        );
    }
}

#[tokio::test]
async fn test_save_contacts_empty_batch_requires_initialized_identity() {
    for initialized in [false, true] {
        let state = crate::storage::StorageState {
            identity_state: Some(IdentityState {
                public_key: initialized.then(|| {
                    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
                }),
                initialized_at: FixedClock.now(),
            }),
            ..Default::default()
        };
        let inner = InMemoryStorage::from_state(state.clone());
        let transactions = Arc::new(AtomicUsize::new(0));
        let sdk = PaykitSdk::with_clock(
            ContactTestStorage {
                inner: inner.clone(),
                transactions: transactions.clone(),
            },
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::new("test-app").unwrap(),
            FixedClock,
        );

        let result = sdk.save_contacts(Vec::new()).await;
        if initialized {
            assert!(result.unwrap().is_empty());
        } else {
            assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        }
        assert_eq!(transactions.load(Ordering::SeqCst), 1);
        assert_eq!(inner.snapshot().unwrap(), state);
    }
}

#[tokio::test]
async fn test_publish_paykit_blob_requires_session() {
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.delete_paykit_profile("profile-revision".into()).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_upload_profile_avatar_rejects_unsupported_content_type() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.upload_profile_avatar(vec![1, 2, 3], "text/plain").await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_delete_paykit_blob_requires_initialized_session() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.delete_paykit_blob("/pub/paykit/blobs/avatar.jpg").await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_fetch_pubky_file_requires_public_storage() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .fetch_pubky_file("pubky://invalid/pub/paykit/blobs/avatar.jpg", 1024)
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
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    label: None,
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::Published,
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.remove_contact(&contact_public_key).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
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
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    label: None,
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
        PaykitSdkConfig {
            public_contact_sharing: PublicContactSharingPolicy::Enabled,
            ..PaykitSdkConfig::new("test-app").unwrap()
        },
        FixedClock,
    );

    let result = sdk.publish_public_contact(contact_public_key.clone()).await;

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
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    label: None,
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::Published,
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.remove_public_contact(contact_public_key.clone()).await;

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

    let result = sdk.remove_public_contact(contact_public_key).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_sync_public_contact_markers_returns_empty_without_pending_markers() {
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
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    label: None,
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::PendingPublication,
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
            public_contact_sharing: PublicContactSharingPolicy::Enabled,
            ..PaykitSdkConfig::new("test-app").unwrap()
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
async fn test_sync_public_contact_markers_preserves_pending_publication_when_sharing_disabled() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    storage
        .transaction({
            let contact_public_key = contact_public_key.clone();
            move |tx| {
                tx.save_contact_record(ContactRecord {
                    public_key: contact_public_key,
                    label: None,
                    profile: None,
                    profile_fetched_at: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    public_contact_marker_status: crate::PublicationStatus::PendingPublication,
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
        crate::PublicationStatus::PendingPublication
    );
    assert!(record.public_contact_last_error.is_none());
}

#[tokio::test]
async fn test_save_contact_requires_initialized_identity() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let contact_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());

    let result = sdk
        .save_contact(ContactUpdate {
            public_key: contact_public_key,
            label: None,
        })
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}
