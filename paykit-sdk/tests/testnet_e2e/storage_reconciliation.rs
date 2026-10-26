//! Real Pubky storage with a faulting HTTP proxy. The proxy tests the SDK's
//! response/cancellation boundaries, not GCS's generation-precondition contract.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{body::Body, extract::State, http, response::Response, Router};
use paykit_sdk::{
    PaykitSdk, PaykitSdkConfig, PubkyLocalSecretKey, PubkyPublicKey, PubkySessionAccess,
    PubkySessionBootstrap, PubkySharedStateStorage, StorageAdapter,
    PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
};
use pubky_testnet::pubky_homeserver::ConfigToml;
use tokio::sync::oneshot;

use crate::harness::{
    build_testnet_with_config, TestnetInstance, TestnetPaymentAdapter, TestnetSessionProvider,
};

const STATE: &str = paykit_lib::PAYKIT_SHARED_STATE_PATH;
const MARKERS: &str = paykit_lib::PAYKIT_SHARED_STATE_WRITE_PATH_PREFIX;

enum Fault {
    TruncateRead,
    Reject(http::StatusCode),
    LoseResponse,
    HoldResponse(oneshot::Sender<()>, oneshot::Receiver<()>),
    HoldLockResponse(oneshot::Sender<()>, oneshot::Receiver<()>),
    InvalidLockGrant(u64),
}

#[derive(Default)]
struct Observations {
    writes: Vec<(String, Vec<u8>)>,
    faults: VecDeque<Fault>,
    lock_timeout_seconds: Option<u64>,
}

#[derive(Clone)]
struct Proxy {
    upstream: String,
    client: reqwest::Client,
    observations: Arc<Mutex<Observations>>,
}

async fn forward(State(proxy): State<Proxy>, request: http::Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 65 * 1024 * 1024)
        .await
        .expect("bounded test request");
    let path = parts.uri.path();
    let (fault, lock_timeout_seconds) = {
        let mut observations = proxy.observations.lock().unwrap();
        if parts.method == http::Method::PUT {
            observations.writes.push((path.into(), bytes.to_vec()));
        }
        let read_fault = matches!(observations.faults.front(), Some(Fault::TruncateRead));
        let lock_fault = matches!(
            observations.faults.front(),
            Some(Fault::HoldLockResponse(..) | Fault::InvalidLockGrant(_))
        );
        let fault = if path.ends_with(STATE)
            && ((lock_fault && parts.method.as_str() == "LOCK")
                || (!lock_fault
                    && ((parts.method == http::Method::PUT && !read_fault)
                        || (parts.method == http::Method::GET && read_fault))))
        {
            observations.faults.pop_front()
        } else {
            None
        };
        (fault, observations.lock_timeout_seconds)
    };
    if let Some(Fault::Reject(status)) = fault {
        return Response::builder()
            .status(status)
            .body(Body::empty())
            .unwrap();
    }
    let mut headers = parts.headers;
    headers.remove(http::header::HOST);
    headers.remove(http::header::CONTENT_LENGTH);
    if parts.method.as_str() == "LOCK" {
        if let Some(seconds) = lock_timeout_seconds {
            headers.insert("timeout", format!("Second-{seconds}").parse().unwrap());
        }
    }
    let response = proxy
        .client
        .request(parts.method, format!("{}{}", proxy.upstream, parts.uri))
        .headers(headers)
        .body(bytes)
        .send()
        .await
        .expect("test homeserver response");
    let status = response.status();
    let mut headers = response.headers().clone();
    let bytes = response.bytes().await.unwrap();
    match fault {
        Some(Fault::TruncateRead) => {
            let half = bytes.slice(..bytes.len() / 2);
            return Response::builder()
                .status(status)
                .body(Body::from_stream(futures_util::stream::iter([
                    Ok(half),
                    Err(std::io::Error::other("interrupted state download")),
                ])))
                .unwrap();
        }
        Some(Fault::LoseResponse) => {
            return Response::builder()
                .status(http::StatusCode::BAD_GATEWAY)
                .body(Body::empty())
                .unwrap();
        }
        Some(Fault::InvalidLockGrant(seconds)) => {
            assert!(status.is_success());
            headers.insert("timeout", format!("Second-{seconds}").parse().unwrap());
        }
        Some(Fault::HoldResponse(entered, release) | Fault::HoldLockResponse(entered, release)) => {
            assert!(status.is_success());
            let _ = entered.send(());
            let _ = release.await;
        }
        _ => {}
    }
    let mut response = Response::builder()
        .status(status)
        .body(Body::from(bytes))
        .unwrap();
    *response.headers_mut() = headers;
    response
}

struct Fixture {
    _testnet: TestnetInstance,
    server: tokio::task::JoinHandle<()>,
    observations: Arc<Mutex<Observations>>,
    access: PubkySessionAccess,
    storage: PubkySharedStateStorage,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(initialize: bool) -> Self {
        Self::with_config(initialize, ConfigToml::minimal_test_config()).await
    }

    async fn with_config(initialize: bool, mut config: ConfigToml) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        // EphemeralTestnet's documented default server key is deterministic.
        let server_key = pubky::Keypair::from_secret(&[0; 32]);
        let tls = server_key.to_rpk_rustls_server_config();
        config.pkdns.public_pubky_tls_port = Some(listener.local_addr().unwrap().port());
        let testnet = build_testnet_with_config(config).await;
        assert_eq!(
            testnet.homeserver_app().public_key(),
            server_key.public_key()
        );
        let observations = Arc::new(Mutex::new(Observations::default()));
        let proxy = Proxy {
            upstream: testnet
                .homeserver_app()
                .icann_http_url()
                .as_str()
                .trim_end_matches('/')
                .into(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            observations: observations.clone(),
        };
        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(
                listener,
                axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls)),
            )
            .unwrap()
            .serve(
                Router::new()
                    .fallback(forward)
                    .with_state(proxy)
                    .into_make_service(),
            )
            .await
            .unwrap();
        });
        let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
        let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
        let client = testnet
            .client_builder()
            .request_timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        let access = PubkySessionBootstrap::with_pubky(
            pubky::Pubky::with_client(client),
            "reconciliation.test",
        )
        .unwrap()
        .with_auth_relay(
            testnet
                .http_relay()
                .local_url()
                .join("inbox")
                .unwrap()
                .as_str(),
        )
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
        if initialize {
            PaykitSdk::new(
                storage.clone(),
                provider,
                TestnetPaymentAdapter::default(),
                PaykitSdkConfig::new("bitkit").unwrap(),
            )
            .initialize()
            .await
            .unwrap();
        }
        Self {
            _testnet: testnet,
            server,
            observations,
            access,
            storage,
        }
    }

    async fn mark(&self) -> String {
        let path = format!("{MARKERS}{}", "a".repeat(64));
        self.access
            .session
            .storage()
            .put(&path, Vec::new())
            .await
            .unwrap();
        path
    }

    async fn bytes(&self) -> Vec<u8> {
        self.access
            .session
            .storage()
            .get(STATE)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec()
    }

    fn fault(&self, fault: Fault) {
        self.observations.lock().unwrap().faults.push_back(fault);
    }

    async fn read(&self) -> paykit_sdk::Result<()> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.storage.transaction(|tx| {
                assert!(tx.load_identity_state().is_some());
                Ok(())
            }),
        )
        .await
        .expect("reconciliation must complete within the test deadline")
    }
}

#[tokio::test]
async fn test_oversized_acquisition_grant_never_enters_operation() {
    let fixture = Fixture::new(true).await;
    let writes_before = fixture.observations.lock().unwrap().writes.len();
    fixture.fault(Fault::InvalidLockGrant(u64::MAX));
    let invoked = AtomicBool::new(false);
    let error = paykit_lib::with_write_lock::<(), paykit_lib::PaykitError, _, _>(
        &fixture.access.session,
        STATE,
        |_| {
            invoked.store(true, Ordering::SeqCst);
            async { Ok(()) }
        },
    )
    .await
    .unwrap_err();
    assert!(!invoked.load(Ordering::SeqCst));
    assert!(!paykit_lib::is_write_conflict(&error));
    assert_eq!(
        fixture.observations.lock().unwrap().writes.len(),
        writes_before
    );
}

#[tokio::test]
async fn test_oversized_renewal_grant_preserves_uncertain_single_write() {
    let fixture = Fixture::new(true).await;
    fixture.observations.lock().unwrap().lock_timeout_seconds = Some(6);
    let original = fixture.bytes().await;
    let writes_before = fixture.observations.lock().unwrap().writes.len();
    let invoked = AtomicBool::new(false);
    let error = tokio::time::timeout(
        Duration::from_secs(8),
        paykit_lib::with_write_lock::<(), paykit_lib::PaykitError, _, _>(
            &fixture.access.session,
            STATE,
            |lock| {
                invoked.store(true, Ordering::SeqCst);
                let fixture = &fixture;
                let original = &original;
                async move {
                    fixture
                        .access
                        .session
                        .storage()
                        .put_locked(&lock, original.clone())
                        .await
                        .unwrap();
                    fixture.fault(Fault::InvalidLockGrant(u64::MAX));
                    std::future::pending::<()>().await;
                    unreachable!("invalid renewal cannot resume an interrupted operation");
                }
            },
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(invoked.load(Ordering::SeqCst));
    assert!(!paykit_lib::is_write_conflict(&error));
    assert_eq!(
        fixture.observations.lock().unwrap().writes.len(),
        writes_before + 1
    );
    assert_eq!(fixture.bytes().await, original);
}

#[tokio::test]
async fn test_expired_acquisition_response_never_enters_shared_state_operation() {
    let fixture = Fixture::new(true).await;
    fixture.observations.lock().unwrap().lock_timeout_seconds = Some(3);
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    fixture.fault(Fault::HoldLockResponse(entered_tx, release_rx));
    let invoked = AtomicBool::new(false);
    let operation = paykit_lib::with_write_lock::<(), paykit_lib::PaykitError, _, _>(
        &fixture.access.session,
        STATE,
        |_| {
            invoked.store(true, Ordering::SeqCst);
            async { Ok(()) }
        },
    );
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => panic!("lock response should be held: {result:?}"),
        _ = entered_rx => {}
    }
    tokio::time::sleep(Duration::from_secs(4)).await;
    release_tx.send(()).unwrap();
    let error = operation.await.unwrap_err();
    assert!(
        matches!(&error, paykit_lib::PaykitError::Transport { context, .. }
        if context == "Pubky write lock response arrived after its safe lease expired")
    );
    assert!(!paykit_lib::is_write_conflict(&error));
    assert!(!invoked.load(Ordering::SeqCst));
    fixture.observations.lock().unwrap().lock_timeout_seconds = None;
    fixture.read().await.unwrap();
}

#[tokio::test]
async fn test_delayed_renewal_stops_operation_after_confirmed_write() {
    let fixture = Fixture::new(true).await;
    fixture.observations.lock().unwrap().lock_timeout_seconds = Some(3);
    let original = fixture.bytes().await;
    let writes_before = fixture.observations.lock().unwrap().writes.len();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let operation_storage = fixture.access.session.storage();
    let operation_fixture = &fixture;
    let original_bytes = original.clone();
    let operation = paykit_lib::with_write_lock::<(), paykit_lib::PaykitError, _, _>(
        &fixture.access.session,
        STATE,
        |lock| async move {
            operation_storage
                .put_locked(&lock, original_bytes)
                .await
                .unwrap();
            operation_fixture
                .observations
                .lock()
                .unwrap()
                .lock_timeout_seconds = Some(6);
            operation_fixture.fault(Fault::HoldLockResponse(entered_tx, release_rx));
            resume_rx.await.unwrap();
            panic!("the operation must not resume after its confirmed lease expires");
        },
    );
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => panic!("renewal response should be held: {result:?}"),
        _ = entered_rx => {}
    }
    // Resume after the original lease but before the longer renewal would expire.
    // Accepting that stale response would let the ready continuation run.
    release_tx.send(()).unwrap();
    resume_tx.send(()).unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let error = tokio::time::timeout(Duration::from_secs(1), operation)
        .await
        .expect("expired confirmation must stop the operation promptly")
        .unwrap_err();
    assert!(!paykit_lib::is_write_conflict(&error));
    assert_eq!(
        fixture.observations.lock().unwrap().writes.len(),
        writes_before + 1
    );
    fixture.observations.lock().unwrap().lock_timeout_seconds = None;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match fixture.read().await {
                Ok(()) => break,
                Err(paykit_sdk::PaykitSdkError::SharedStateBusy { .. }) => {
                    tokio::time::sleep(Duration::from_millis(100)).await
                }
                Err(error) => panic!("unexpected recovery failure: {error}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.bytes().await, original);
}

#[tokio::test]
async fn test_pending_state_reconciles_at_storage_quota() {
    let mut config = ConfigToml::minimal_test_config();
    config.storage.default_quota_mb = Some(1);
    let fixture = Fixture::with_config(true, config).await;
    let marker = fixture.mark().await;
    let original = fixture.bytes().await;
    let storage = fixture.access.session.storage();
    let entries = storage
        .list("/pub/")
        .unwrap()
        .shallow(false)
        .limit(100)
        .send()
        .await
        .unwrap();
    // Homeserver accounts for 256 metadata bytes per file, including empty files.
    let mut used = 0;
    for entry in entries {
        used += storage
            .get(entry.path.as_str())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .len()
            + 256;
    }
    storage
        .put(
            "/pub/paykit/blobs/quota-padding",
            vec![0; 1024 * 1024 - used - 256],
        )
        .await
        .unwrap();
    let rejected = storage
        .put(format!("{MARKERS}{}", "b".repeat(64)), Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(
        rejected,
        pubky::Error::Request(pubky::errors::RequestError::Server { status, .. })
            if status == pubky::StatusCode::INSUFFICIENT_STORAGE
    ));

    fixture.read().await.unwrap();

    assert_eq!(fixture.bytes().await, original);
    assert!(storage.get(&marker).await.is_err());
}

#[tokio::test]
async fn test_pending_state_is_republished_unchanged_before_use() {
    let fixture = Fixture::new(true).await;
    let original = fixture.bytes().await;
    fixture.observations.lock().unwrap().writes.clear();
    fixture.read().await.unwrap();
    assert!(
        fixture.observations.lock().unwrap().writes.is_empty(),
        "ordinary reads must not write"
    );
    let marker = fixture.mark().await;
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture.storage.transaction(|_| {
            let observations = fixture.observations.lock().unwrap();
            let state_writes: Vec<_> = observations
                .writes
                .iter()
                .filter(|(path, _)| path.ends_with(STATE))
                .collect();
            assert_eq!(state_writes.len(), 1, "publish before exposing any state");
            assert_eq!(state_writes[0].1, original);
            Ok(())
        }),
    )
    .await
    .expect("pending state must reconcile within the test deadline")
    .unwrap();
    assert_eq!(fixture.bytes().await, original);
    assert!(fixture.access.session.storage().get(&marker).await.is_err());
}

#[tokio::test]
async fn test_reconciliation_failure_keeps_state_unavailable_and_markers_pending() {
    let fixture = Fixture::new(true).await;
    let original = fixture.bytes().await;
    fixture.observations.lock().unwrap().writes.clear();
    for fault in [
        Fault::Reject(http::StatusCode::CONFLICT),
        Fault::Reject(http::StatusCode::PRECONDITION_FAILED),
        Fault::Reject(http::StatusCode::INSUFFICIENT_STORAGE),
        Fault::Reject(http::StatusCode::TOO_MANY_REQUESTS),
        Fault::Reject(http::StatusCode::INTERNAL_SERVER_ERROR),
        Fault::LoseResponse,
    ] {
        let conflict = matches!(fault, Fault::Reject(status) if status == http::StatusCode::CONFLICT || status == http::StatusCode::PRECONDITION_FAILED);
        let quota = matches!(fault, Fault::Reject(http::StatusCode::INSUFFICIENT_STORAGE));
        let marker = fixture.mark().await;
        fixture.fault(fault);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            fixture.storage.transaction::<(), _>(|_| {
                panic!("unconfirmed reconciliation must not expose state")
            }),
        )
        .await
        .unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.is_concurrent_update(), conflict, "{error}");
        if quota {
            assert!(
                matches!(&error, paykit_sdk::PaykitSdkError::Transport { context, .. }
                if context == "Pubky shared-state reconciliation rejected by storage quota")
            );
        }
        assert!(fixture.access.session.storage().get(&marker).await.is_ok());
        fixture.read().await.unwrap();
        assert_eq!(fixture.bytes().await, original);
        assert!(fixture.access.session.storage().get(&marker).await.is_err());
    }
    let observations = fixture.observations.lock().unwrap();
    assert!(observations
        .writes
        .iter()
        .filter(|(path, _)| path.contains(MARKERS))
        .all(|(path, _)| path.ends_with(&"a".repeat(64))));
}

#[tokio::test]
async fn test_cancelled_reconciliation_preserves_pending_recovery() {
    let fixture = Fixture::new(true).await;
    let marker = fixture.mark().await;
    let (entered, arrived) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    fixture.fault(Fault::HoldResponse(entered, wait));
    let storage = fixture.storage.clone();
    let operation = tokio::spawn(async move {
        storage
            .transaction::<(), _>(|_| panic!("cancelled reconciliation must not expose state"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), arrived)
        .await
        .unwrap()
        .unwrap();
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    assert!(fixture.access.session.storage().get(&marker).await.is_ok());
    // Cancellation retains the server lease until expiry. Recovery must then
    // reconcile the pending state before exposing it to another operation.
    tokio::time::timeout(Duration::from_secs(75), async {
        loop {
            match fixture.read().await {
                Ok(()) => break,
                Err(paykit_sdk::PaykitSdkError::SharedStateBusy { .. }) => {
                    tokio::time::sleep(Duration::from_millis(100)).await
                }
                Err(error) => panic!("unexpected recovery failure: {error}"),
            }
        }
    })
    .await
    .unwrap();
    assert!(fixture.access.session.storage().get(&marker).await.is_err());
}

#[tokio::test]
async fn test_pending_first_initialization_can_reconcile_absence() {
    let fixture = Fixture::new(false).await;
    let marker = fixture.mark().await;
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture.storage.transaction(|tx| {
            assert!(tx.load_identity_state().is_none());
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(fixture.access.session.storage().get(&marker).await.is_err());
    assert!(fixture.access.session.storage().get(STATE).await.is_err());
}

#[tokio::test]
async fn test_incomplete_state_read_cannot_reconcile_or_expose_state() {
    let fixture = Fixture::new(true).await;
    let marker = fixture.mark().await;
    fixture.observations.lock().unwrap().writes.clear();
    fixture.fault(Fault::TruncateRead);
    let error = fixture
        .storage
        .transaction::<(), _>(|_| panic!("an incomplete download must not expose state"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        paykit_sdk::PaykitSdkError::Transport { .. }
    ));
    assert!(fixture.observations.lock().unwrap().writes.is_empty());
    assert!(fixture.access.session.storage().get(&marker).await.is_ok());
    fixture.read().await.unwrap();
}

#[tokio::test]
async fn test_lost_commit_response_invalidates_scope_and_recovers_committed_state() {
    let fixture = Fixture::new(true).await;
    let initialized_at = fixture
        .storage
        .transaction(|tx| Ok(tx.load_identity_state().unwrap().initialized_at))
        .await
        .unwrap();
    fixture.fault(Fault::LoseResponse);
    fixture
        .storage
        .with_operation(async {
            let result = fixture
                .storage
                .transaction(|tx| {
                    let mut identity = tx.load_identity_state().unwrap();
                    identity.initialized_at += chrono::Duration::seconds(1);
                    tx.save_identity_state(identity);
                    Ok(())
                })
                .await;
            assert!(matches!(
                result,
                Err(paykit_sdk::PaykitSdkError::Transport { .. })
            ));
            assert!(fixture
                .storage
                .transaction::<(), _>(|_| {
                    panic!("catching an uncertain write must not restore authority")
                })
                .await
                .is_err());
            Ok(())
        })
        .await
        .unwrap();
    let committed = fixture.bytes().await;
    fixture.read().await.unwrap();
    assert_eq!(
        fixture.bytes().await,
        committed,
        "recover the exact committed ciphertext"
    );
    fixture
        .storage
        .transaction(|tx| {
            assert_eq!(
                tx.load_identity_state().unwrap().initialized_at,
                initialized_at + chrono::Duration::seconds(1)
            );
            Ok(())
        })
        .await
        .unwrap();
}
