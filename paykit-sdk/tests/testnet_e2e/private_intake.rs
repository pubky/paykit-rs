use std::{
    any::Any,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use paykit_sdk::{
    storage::StorageTransactionCallback, InMemoryStorage, PaykitSdk, PaykitSdkConfig,
    PrivateIntakeConfig, PrivateIntakeTransportState, Result, StorageAdapter,
};

use crate::harness::{linked_two_party, TestUser, TestnetSessionProvider, TwoParty};

const RETAINED_PRIVATE_STREAM_ITEM_LIMIT: usize = 4096;

#[derive(Clone)]
struct RejectBufferedDrainCommit {
    inner: InMemoryStorage,
    reject: Arc<AtomicBool>,
}

#[async_trait]
impl StorageAdapter for RejectBufferedDrainCommit {
    async fn transaction_erased<'a>(
        &self,
        callback: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        let reject = Arc::clone(&self.reject);
        self.inner
            .transaction_erased(Box::new(move |tx| {
                let before = tx.export_storage_state().private_stream_items.len();
                let result = callback(tx)?;
                if reject.load(Ordering::SeqCst)
                    && tx.export_storage_state().private_stream_items.len() > before
                {
                    return Err(paykit_sdk::PaykitSdkError::Storage {
                        context: "injected buffered drain commit failure".into(),
                        source: None,
                    });
                }
                Ok(result)
            }))
            .await
    }
}

async fn publish_private_lists(sender: &TestUser, receiver: &TestUser, count: usize) {
    for _ in 0..count {
        sender
            .sdk
            .clear_private_payment_list(receiver.public_key.clone())
            .await
            .unwrap();
        let report = sender
            .sdk
            .process_outbound_private_messages(receiver.public_key.clone())
            .await
            .unwrap();
        assert_eq!(report.sent.len(), 1);
    }
}

async fn wait_for_buffered(
    storage: &InMemoryStorage,
    peer: &paykit_sdk::PubkyPublicKey,
    count: usize,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let buffered = storage
                .snapshot()
                .unwrap()
                .private_intake_peers
                .get(peer)
                .map_or(0, |state| state.buffered_packets.len());
            if buffered >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("private intake should durably buffer expected packets");
}

#[tokio::test]
async fn test_buffered_receive_replays_atomic_commit_once_across_sdk_handles() {
    let pair = linked_two_party().await;
    publish_private_lists(&pair.bob, &pair.alice, 1).await;

    let reject = Arc::new(AtomicBool::new(false));
    let storage = RejectBufferedDrainCommit {
        inner: pair.alice.storage.clone(),
        reject: Arc::clone(&reject),
    };
    let sdk = Arc::new(PaykitSdk::new(
        storage.clone(),
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let alternate = Arc::new(PaykitSdk::new(
        storage,
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    wait_for_buffered(&pair.alice.storage, &pair.bob.public_key, 1).await;
    assert!(pair
        .alice
        .storage
        .snapshot()
        .unwrap()
        .private_stream_items
        .is_empty());

    reject.store(true, Ordering::SeqCst);
    let failed = sdk
        .receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor)
        .await;
    assert!(failed.is_err());
    let after_failure = pair.alice.storage.snapshot().unwrap();
    assert!(after_failure.private_stream_items.is_empty());
    assert_eq!(
        after_failure.private_intake_peers[&pair.bob.public_key]
            .buffered_packets
            .len(),
        1
    );

    reject.store(false, Ordering::SeqCst);
    let (first, second) = tokio::join!(
        sdk.receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor),
        alternate.receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor),
    );
    let mut processed = 0;
    let mut excluded = 0;
    for result in [first, second] {
        match result {
            Ok(report) => processed += report.report.stream_item_ids.len(),
            Err(error) if error.is_concurrent_update() => excluded += 1,
            Err(error) => panic!("unexpected alternate-handle drain error: {error}"),
        }
    }
    assert_eq!(
        processed, 1,
        "alternate handles must not process one counter twice"
    );
    assert!(excluded <= 1);
    let state = pair.alice.storage.snapshot().unwrap();
    assert_eq!(state.private_stream_items.len(), 1);
    assert!(state.private_intake_peers[&pair.bob.public_key]
        .buffered_packets
        .is_empty());
    supervisor.stop().await;
}

#[tokio::test]
async fn test_buffered_receive_is_bounded_and_drains_while_transport_stale() {
    let TwoParty {
        _testnet,
        alice,
        bob,
    } = linked_two_party().await;
    let count = paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT + 1;
    publish_private_lists(&bob, &alice, count).await;
    let sdk = Arc::new(PaykitSdk::new(
        alice.storage.clone(),
        TestnetSessionProvider::new(alice.access.clone()),
        alice.adapter.clone(),
        PaykitSdkConfig::new(alice.app_id.clone()).unwrap(),
    ));
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    wait_for_buffered(&alice.storage, &bob.public_key, count).await;

    drop(_testnet);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if supervisor.health().await.peers.get(&bob.public_key)
                == Some(&PrivateIntakeTransportState::Stale)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("closed homeserver should make private intake stale");

    let first = sdk
        .receive_buffered_private_messages(bob.public_key.clone(), &supervisor)
        .await
        .unwrap();
    assert_eq!(
        first.report.stream_item_ids.len(),
        paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT
    );
    assert_eq!(first.remaining_buffered_packets, 1);
    assert_eq!(
        first.freshness.transport_state,
        PrivateIntakeTransportState::Stale
    );
    let second = sdk
        .receive_buffered_private_messages(bob.public_key.clone(), &supervisor)
        .await
        .unwrap();
    assert_eq!(second.report.stream_item_ids.len(), 1);
    assert_eq!(second.remaining_buffered_packets, 0);
    supervisor.stop().await;
}

#[tokio::test]
async fn test_buffered_receive_retention_refusal_keeps_packet_and_checkpoint() {
    let pair = linked_two_party().await;
    publish_private_lists(&pair.bob, &pair.alice, 1).await;
    pair.alice
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    let mut state = pair.alice.storage.snapshot().unwrap();
    let retained = state.private_stream_items[0].clone();
    state.private_stream_items = vec![retained; RETAINED_PRIVATE_STREAM_ITEM_LIMIT];
    state.next_private_stream_item_id = state.private_stream_items.len() as u64;
    let storage = InMemoryStorage::from_state(state);
    publish_private_lists(&pair.bob, &pair.alice, 1).await;

    let sdk = Arc::new(PaykitSdk::new(
        storage.clone(),
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    wait_for_buffered(&storage, &pair.bob.public_key, 1).await;
    let before = storage.snapshot().unwrap();

    let error = sdk
        .receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor)
        .await
        .unwrap_err();

    assert!(error.is_retention_limit_reached());
    let after = storage.snapshot().unwrap();
    assert_eq!(after.private_stream_items, before.private_stream_items);
    assert_eq!(
        after.encrypted_link_states[&pair.bob.public_key],
        before.encrypted_link_states[&pair.bob.public_key]
    );
    assert_eq!(
        after.private_intake_peers[&pair.bob.public_key].buffered_packets,
        before.private_intake_peers[&pair.bob.public_key].buffered_packets
    );
    supervisor.stop().await;
}
