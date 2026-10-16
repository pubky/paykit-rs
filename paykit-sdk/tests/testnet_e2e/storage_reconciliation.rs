//! Real Pubky storage with a faulting HTTP proxy. The proxy tests the SDK's
//! response/cancellation boundaries, not GCS's generation-precondition contract.

use std::collections::VecDeque;
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
}

#[derive(Default)]
struct Observations {
    writes: Vec<(String, Vec<u8>)>,
    faults: VecDeque<Fault>,
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
    let fault = {
        let mut observations = proxy.observations.lock().unwrap();
        if parts.method == http::Method::PUT {
            observations.writes.push((path.into(), bytes.to_vec()));
        }
        let read_fault = matches!(observations.faults.front(), Some(Fault::TruncateRead));
        if path.ends_with(STATE)
            && ((parts.method == http::Method::PUT && !read_fault)
                || (parts.method == http::Method::GET && read_fault))
        {
            observations.faults.pop_front()
        } else {
            None
        }
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
    let response = proxy
        .client
        .request(parts.method, format!("{}{}", proxy.upstream, parts.uri))
        .headers(headers)
        .body(bytes)
        .send()
        .await
        .expect("test homeserver response");
    let status = response.status();
    let headers = response.headers().clone();
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
        Some(Fault::HoldResponse(entered, release)) => {
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        // EphemeralTestnet's documented default server key is deterministic.
        let server_key = pubky::Keypair::from_secret(&[0; 32]);
        let tls = server_key.to_rpk_rustls_server_config();
        let mut config = ConfigToml::minimal_test_config();
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
        .expect("reconciliation must not impose a timed cooldown")
    }
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
    .expect("pending state must reconcile without a five-minute delay")
    .unwrap();
    assert_eq!(fixture.bytes().await, original);
    assert!(fixture.access.session.storage().get(&marker).await.is_err());
}

#[tokio::test]
async fn test_reconciliation_failure_keeps_state_unavailable_and_markers_pending() {
    let fixture = Fixture::new(true).await;
    let original = fixture.bytes().await;
    for fault in [
        Fault::Reject(http::StatusCode::CONFLICT),
        Fault::Reject(http::StatusCode::PRECONDITION_FAILED),
        Fault::Reject(http::StatusCode::INTERNAL_SERVER_ERROR),
        Fault::LoseResponse,
    ] {
        let conflict = matches!(fault, Fault::Reject(status) if status == http::StatusCode::CONFLICT || status == http::StatusCode::PRECONDITION_FAILED);
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
        assert!(fixture.access.session.storage().get(&marker).await.is_ok());
        fixture.read().await.unwrap();
        assert_eq!(fixture.bytes().await, original);
        assert!(fixture.access.session.storage().get(&marker).await.is_err());
    }
    let observations = fixture.observations.lock().unwrap();
    let attempts: Vec<_> = observations
        .writes
        .iter()
        .filter(|(path, _)| path.contains(MARKERS) && !path.ends_with(&"a".repeat(64)))
        .map(|(path, _)| path)
        .collect();
    let unique: std::collections::HashSet<_> = attempts.iter().collect();
    assert_eq!(
        attempts.len(),
        unique.len(),
        "identical ciphertext must use distinct attempt markers"
    );
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
    // Cancellation retains the normal server lease until its expiry; it must
    // not retain the former five-minute uncertain-write delay as well.
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
