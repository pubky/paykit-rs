use super::*;
use crate::domain::receipts::{receipt_access_key_hash, ReceiptAccessRecord, ReceiptRecord};
use crate::storage::{run_storage_state_transaction, StorageState, StorageTransactionCallback};

async fn seed_private_list_item(storage: &InMemoryStorage, counterparty: PubkyPublicKey) {
    persist_private_stream_batch(
        storage,
        counterparty,
        receiver_path(),
        vec![private_list_message("ln-private")],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
}

async fn corrupt_derived_classification(storage: &InMemoryStorage) {
    let mut state = storage.snapshot().unwrap();
    let item = state.private_stream_items.last_mut().unwrap();
    item.parse_status = crate::PrivateStreamParseStatus::MalformedRecognized;
    item.parse_error = Some("stale serde detail".into());
    storage
        .transaction(move |tx| {
            tx.replace_storage_state(crate::backup::ValidatedStorageState::new(state));
            Ok(())
        })
        .await
        .unwrap();
}

fn assert_derived_state_matches(state: &StorageState, pristine: &StorageState) {
    assert_eq!(state.private_stream_items, pristine.private_stream_items);
    assert_eq!(state.event_dedup_records, pristine.event_dedup_records);
    assert_eq!(
        state.receipt_access_records,
        pristine.receipt_access_records
    );
}

#[tokio::test]
async fn test_initialize_normalizes_stale_classifications() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_list_item(&storage, counterparty).await;
    let pristine = storage.snapshot().unwrap();
    corrupt_derived_classification(&storage).await;
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    sdk.initialize().await.unwrap();

    assert_derived_state_matches(&storage.snapshot().unwrap(), &pristine);
}

#[tokio::test]
async fn test_direct_read_without_initialize_normalizes() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            local_pubky_public_key: Some(local_public_key),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    seed_private_list_item(&storage, counterparty.clone()).await;
    let pristine = storage.snapshot().unwrap();
    corrupt_derived_classification(&storage).await;
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let view = sdk
        .current_private_payment_list(&counterparty, &receiver_path())
        .await
        .unwrap()
        .expect("normalized Private Payment List item must project again");

    assert_eq!(
        view.payment_endpoints.get("btc-lightning-bolt11"),
        Some(&"ln-private".to_owned())
    );
    assert_derived_state_matches(&storage.snapshot().unwrap(), &pristine);
}

#[tokio::test]
async fn test_repeated_initialization_is_noop() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_list_item(&storage, counterparty).await;
    corrupt_derived_classification(&storage).await;
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let first = sdk.initialize().await.unwrap();
    let after_first = storage.snapshot().unwrap();
    let second = sdk.initialize().await.unwrap();

    assert_eq!(first, second);
    assert_eq!(storage.snapshot().unwrap(), after_first);

    // The memo only avoids rescanning within one process: corruption
    // introduced after a successful pass is not repaired by repeated
    // initialization, only by a fresh runtime.
    corrupt_derived_classification(&storage).await;
    let corrupted = storage.snapshot().unwrap();
    sdk.initialize().await.unwrap();
    assert_eq!(storage.snapshot().unwrap(), corrupted);
}

#[tokio::test]
async fn test_live_normalization_drops_orphaned_receipt_and_backup_roundtrips() {
    // An older classifier indexed an out-of-scope Receipt Access event and
    // cached the retrieved Receipt. Live normalization must drop the cached
    // Receipt together with its index, so the very next export produces a
    // backup that still restores.
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            local_pubky_public_key: Some(local_public_key.clone()),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let location = paykit_lib::ReceiptAccess::location(&other_receiver_path(), &receipt_id);
    let key = paykit_lib::ReceiptDecryptionKey::generate()
        .as_str()
        .to_owned();
    let raw_json = format!(
        r#"{{"version":1,"kind":"paykit.receipt_access","event_id":"{event_id}","receipt_id":"{}","payment_reference":"invoice-2026-0001","location":"{location}","key":"{key}"}}"#,
        receipt_id.as_str()
    );
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![PrivateApplicationMessage {
            version: Some(1),
            kind: Some("paykit.receipt_access".into()),
            raw_json: raw_json.clone(),
        }],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let mut state = storage.snapshot().unwrap();
    let item = state.private_stream_items.last_mut().unwrap();
    item.parse_status = crate::PrivateStreamParseStatus::Valid;
    item.parse_error = None;
    let event_key = (counterparty.clone(), receiver_path(), event_id.to_owned());
    state.event_dedup_records.insert(
        event_key.clone(),
        EventDedupRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 0,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        },
    );
    state.receipt_access_records.insert(
        event_key,
        ReceiptAccessRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            stream_item_id: 0,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: receipt_id.as_str().into(),
            payment_reference: "invoice-2026-0001".into(),
            payment_request_id: None,
            billing_period: None,
            location: location.clone(),
            key: key.clone(),
            retrieval_status: crate::ReceiptRetrievalStatus::Retrieved,
            retrieval_attempted_at: Some(FixedClock.now()),
            retrieved_at: Some(FixedClock.now()),
            last_retrieval_error: None,
            received_at: FixedClock.now(),
        },
    );
    state.receipt_records.insert(
        (
            counterparty.clone(),
            receiver_path(),
            receipt_id.as_str().to_owned(),
        ),
        ReceiptRecord {
            issuer: counterparty,
            issuer_receiver_path: receiver_path(),
            receipt_access_event_id: event_id.into(),
            receipt_access_key_hash: receipt_access_key_hash(&key),
            receipt_id: receipt_id.as_str().into(),
            payment_reference: "invoice-2026-0001".into(),
            payment_request_id: None,
            billing_period: None,
            recipient_public_key: local_public_key,
            payment_endpoint_identifier: None,
            amount: None,
            metadata: serde_json::Map::new(),
            location,
            retrieved_at: FixedClock.now(),
        },
    );
    storage
        .transaction(move |tx| {
            tx.replace_storage_state(crate::backup::ValidatedStorageState::new(state));
            Ok(())
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new(receiver_path()),
        FixedClock,
    );

    let backup = sdk.export_backup_state().await.unwrap();

    let live = storage.snapshot().unwrap();
    assert!(live.event_dedup_records.is_empty());
    assert!(live.receipt_access_records.is_empty());
    assert!(live.receipt_records.is_empty());
    assert!(backup.receipt_records.is_empty());

    let restored = InMemoryStorage::new();
    crate::backup::restore_backup_state(&restored, backup)
        .await
        .unwrap();
    assert!(restored.snapshot().unwrap().receipt_records.is_empty());
}

#[derive(Clone)]
struct CommitFailStorage {
    inner: InMemoryStorage,
    fail_commits: Arc<Mutex<bool>>,
}

#[async_trait]
impl StorageAdapter for CommitFailStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        if *self.fail_commits.lock().unwrap() {
            // Run the closure against a discarded copy so the commit fails
            // after in-transaction mutations succeeded; stored state must
            // remain unchanged, matching the adapter rollback contract.
            let state = self.inner.snapshot()?;
            let _ = run_storage_state_transaction(state, f)?;
            return Err(PaykitSdkError::Storage {
                context: "commit failed".into(),
                source: None,
            });
        }
        self.inner.transaction_erased(f).await
    }
}

#[tokio::test]
async fn test_normalization_rolls_back_on_storage_failure() {
    let inner = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_list_item(&inner, counterparty).await;
    let pristine = inner.snapshot().unwrap();
    corrupt_derived_classification(&inner).await;
    let corrupted = inner.snapshot().unwrap();
    let fail_commits = Arc::new(Mutex::new(true));
    let sdk = PaykitSdk::with_clock(
        CommitFailStorage {
            inner: inner.clone(),
            fail_commits: Arc::clone(&fail_commits),
        },
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.payment_requests().await;
    assert!(matches!(result, Err(PaykitSdkError::Storage { .. })));
    assert_eq!(inner.snapshot().unwrap(), corrupted);

    // The memo flag is only set after a committed pass, so the next guarded
    // call retries and normalizes.
    *fail_commits.lock().unwrap() = false;
    sdk.payment_requests().await.unwrap();
    assert_derived_state_matches(&inner.snapshot().unwrap(), &pristine);
}
