use std::{
    any::Any,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use async_trait::async_trait;
use paykit_sdk::{
    load_encrypted_link_state, storage::StorageTransactionCallback, EncryptedLinkHandshakeRole,
    InMemoryStorage, LinkedPeerState, PaykitIdentitySecretKey, PaykitSdk, PaykitSdkConfig,
    PaykitSdkError, PubkyPublicKey, PubkySessionAccess, PubkySessionProvider, Result,
    StorageAdapter,
};
use pubky_testnet::pubky::Keypair;
use tokio::sync::oneshot;

use crate::harness::{
    build_testnet, drive_link_to_linked, two_party, TestUser, TestnetSessionProvider,
};

#[tokio::test]
async fn test_link_handshake_two_party_reaches_linked() {
    let pair = two_party().await;

    let initiated = pair
        .alice
        .sdk
        .initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .expect("initiating the handshake should succeed");
    assert_eq!(initiated.state, LinkedPeerState::Linking);
    assert_eq!(
        initiated.handshake_role,
        Some(EncryptedLinkHandshakeRole::Initiator)
    );

    let accepted = pair
        .bob
        .sdk
        .accept_link_with_peer(pair.alice.public_key.clone())
        .await
        .expect("accepting the handshake should succeed");
    assert_eq!(accepted.state, LinkedPeerState::Linking);
    assert_eq!(
        accepted.handshake_role,
        Some(EncryptedLinkHandshakeRole::Responder)
    );

    drive_link_to_linked(&pair.alice, &pair.bob).await;

    let peers = pair
        .alice
        .sdk
        .linked_peers()
        .await
        .expect("loading linked peers should succeed");
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].state, LinkedPeerState::Linked);

    // The durable link state holds an active link snapshot and no leftover
    // handshake state.
    let link_state = load_encrypted_link_state(&pair.alice.storage, &pair.bob.public_key)
        .await
        .expect("loading link state should succeed")
        .expect("link state should exist after handshake completion");
    assert!(link_state.link_snapshot.is_some());
    assert!(link_state.handshake_snapshot.is_none());
    assert!(link_state.handshake_role.is_none());
}

#[tokio::test]
async fn test_advance_link_handshake_without_started_handshake_fails() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    let stranger = PubkyPublicKey::from_public_key(&Keypair::random().public_key());

    let err = user
        .sdk
        .advance_link_handshake(stranger)
        .await
        .expect_err("advancing without stored handshake state must fail");
    assert!(
        matches!(err, PaykitSdkError::RecoveryRequired { .. }),
        "unexpected error: {err:?}"
    );
}

struct PausedLinkCheckpointStorage {
    inner: InMemoryStorage,
    counterparty: PubkyPublicKey,
    pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

#[async_trait]
impl StorageAdapter for PausedLinkCheckpointStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        let result = self.inner.transaction_erased(f).await?;
        let has_link = self
            .inner
            .snapshot()?
            .encrypted_link_states
            .get(&self.counterparty)
            .is_some_and(|state| state.link_snapshot.is_some());
        let pause = if has_link {
            self.pause.lock().unwrap().take()
        } else {
            None
        };
        if let Some((ready, resume)) = pause {
            ready
                .send(())
                .expect("checkpoint observer should remain live");
            resume.await.expect("checkpoint should be released");
        }
        Ok(result)
    }
}

struct CountingSessionProvider {
    inner: TestnetSessionProvider,
    loads: Arc<AtomicUsize>,
}

#[async_trait]
impl PubkySessionProvider for CountingSessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load_session_access().await
    }

    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
        self.inner.load_public_storage().await
    }

    async fn clear_session_access(&self) -> Result<()> {
        self.inner.clear_session_access().await
    }
}

#[tokio::test]
async fn test_key_rotation_waits_for_handshake_checkpoint_without_nested_session_reads() {
    let pair = two_party().await;
    let (checkpoint_ready, checkpoint_reached) = oneshot::channel();
    let (resume_checkpoint, checkpoint_resume) = oneshot::channel();
    let session_loads = Arc::new(AtomicUsize::new(0));
    let sdk = PaykitSdk::new(
        PausedLinkCheckpointStorage {
            inner: pair.alice.storage.clone(),
            counterparty: pair.bob.public_key.clone(),
            pause: Mutex::new(Some((checkpoint_ready, checkpoint_resume))),
        },
        CountingSessionProvider {
            inner: TestnetSessionProvider::new(pair.alice.access.clone()),
            loads: Arc::clone(&session_loads),
        },
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    sdk.initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.bob
        .sdk
        .accept_link_with_peer(pair.alice.public_key.clone())
        .await
        .unwrap();

    let handshake = async {
        for _ in 0..8 {
            let report = sdk
                .advance_link_handshake(pair.bob.public_key.clone())
                .await
                .unwrap();
            if report.state == LinkedPeerState::Linked {
                return report;
            }
            pair.bob
                .sdk
                .advance_link_handshake(pair.alice.public_key.clone())
                .await
                .unwrap();
        }
        panic!("handshake should reach its linked-state checkpoint");
    };
    tokio::pin!(handshake);
    tokio::select! {
        reached = tokio::time::timeout(Duration::from_secs(30), checkpoint_reached) => {
            reached.expect("handshake checkpoint timed out").unwrap();
        }
        _ = &mut handshake => panic!("handshake must wait for its checkpoint"),
    }

    let loads_before_rotation = session_loads.load(Ordering::SeqCst);
    let replacement = PaykitIdentitySecretKey::new([42; 32], 2).unwrap();
    let rotation = sdk.rotate_paykit_identity_key(replacement);
    tokio::pin!(rotation);
    // Poll once to enqueue the writer before resuming handshake cleanup.
    assert!(matches!(
        rotation
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    assert_eq!(
        session_loads.load(Ordering::SeqCst),
        loads_before_rotation,
        "rotation must wait before loading session keys"
    );
    resume_checkpoint.send(()).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(10), &mut handshake)
        .await
        .expect("handshake cleanup must not reacquire a read behind the rotation writer");
    assert_eq!(report.state, LinkedPeerState::Linked);
    assert_eq!(session_loads.load(Ordering::SeqCst), loads_before_rotation);

    tokio::time::timeout(Duration::from_secs(30), rotation)
        .await
        .expect("rotation should finish after handshake cleanup")
        .unwrap();
    let state = pair.alice.storage.snapshot().unwrap();
    assert!(state.encrypted_link_states.is_empty());
    assert_eq!(
        state.linked_peers[&pair.bob.public_key].state,
        LinkedPeerState::RecoveryRequired
    );
}
