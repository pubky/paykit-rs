use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::State, http::Request, response::Response, Router};
use pubky::{Capability, ClientId, GrantId, Keypair};
use tokio::sync::Notify;

use super::*;

const TEST_BEARER: &str = "confidential-original-bearer";

fn info() -> GrantSessionInfo {
    let now = chrono::Utc::now().timestamp() as u64;
    GrantSessionInfo {
        homeserver: Keypair::random().public_key(),
        pubky: Keypair::random().public_key(),
        client_id: ClientId::new("bitkit.test").unwrap(),
        capabilities: vec![Capability::root()],
        grant_id: GrantId::generate(),
        token_expires_at: now + 3600,
        grant_expires_at: now + 7200,
        created_at: now,
    }
}

fn observer(info: GrantSessionInfo) -> PubkyGrantObserver {
    PubkyGrantObserver {
        client: PubkyHttpClient::builder()
            .pkarr(|builder| {
                builder
                    .no_default_network()
                    .relays(&["http://127.0.0.1:1"])
                    .unwrap()
            })
            .build()
            .unwrap(),
        session: info,
        bearer: Zeroizing::new(TEST_BEARER.into()),
    }
}

#[derive(Clone)]
struct Reply {
    status: StatusCode,
    body: Vec<u8>,
    owner: String,
    requests: Arc<Mutex<Vec<(Method, String)>>>,
    entered: Arc<Notify>,
    hold: Option<Arc<Notify>>,
}

async fn respond(State(reply): State<Reply>, request: Request<Body>) -> Response {
    assert_eq!(request.headers().get_all("pubky-host").iter().count(), 1);
    assert_eq!(request.headers()["pubky-host"], reply.owner);
    assert_eq!(
        request.headers()["authorization"],
        format!("Bearer {TEST_BEARER}")
    );
    reply
        .requests
        .lock()
        .unwrap()
        .push((request.method().clone(), request.uri().path().into()));
    reply.entered.notify_one();
    if let Some(hold) = reply.hold {
        hold.notified().await;
    }
    // Chunked responses exercise the bounded reader without Content-Length.
    Response::builder()
        .status(reply.status)
        .body(Body::from_stream(futures_util::stream::iter(
            reply
                .body
                .chunks(1024)
                .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()))
                .collect::<Vec<_>>(),
        )))
        .unwrap()
}

async fn server(reply: Reply) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/auth/grant/session",
        listener.local_addr().unwrap()
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(respond).with_state(reply))
            .await
            .unwrap();
    });
    (url, task)
}

#[tokio::test]
async fn test_observation_requires_original_binding_and_current_expiry() {
    let original = info();
    let mut replies = vec![(
        StatusCode::OK,
        serde_json::to_vec(&original).unwrap(),
        PubkyGrantObservation::Active,
    )];
    for field in 0..10 {
        let mut changed = original.clone();
        match field {
            0 => changed.grant_id = GrantId::generate(),
            1 => changed.pubky = Keypair::random().public_key(),
            2 => changed.client_id = ClientId::new("replacement.test").unwrap(),
            3 => changed.homeserver = Keypair::random().public_key(),
            4 => changed.capabilities.clear(),
            5 => changed.token_expires_at = 0,
            6 => changed.grant_expires_at = 0,
            7 => changed.created_at += 1,
            8 => changed.token_expires_at += 1,
            9 => changed.grant_expires_at += 1,
            _ => unreachable!(),
        }
        replies.push((
            StatusCode::OK,
            serde_json::to_vec(&changed).unwrap(),
            if field < 7 {
                PubkyGrantObservation::Unknown
            } else {
                PubkyGrantObservation::Active
            },
        ));
    }
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::NOT_FOUND,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        replies.push((
            status,
            serde_json::to_vec(&original).unwrap(),
            PubkyGrantObservation::Unknown,
        ));
    }
    for body in [
        b"invalid metadata".to_vec(),
        vec![b' '; MAX_SESSION_INFO_BYTES + 1],
    ] {
        replies.push((StatusCode::OK, body, PubkyGrantObservation::Unknown));
    }
    for (status, body, expected) in replies {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, task) = server(Reply {
            status,
            body,
            owner: original.pubky.z32(),
            requests: Arc::clone(&requests),
            entered: Arc::new(Notify::new()),
            hold: None,
        })
        .await;
        let captured = observer(original.clone());
        let builder = reqwest::Client::new()
            .get(&url)
            .header("pubky-host", "wrong-inferred-homeserver");
        assert_eq!(captured.observe_request(builder).await, expected);
        assert_eq!(
            *requests.lock().unwrap(),
            vec![(Method::GET, "/auth/grant/session".into())]
        );
        task.abort();
    }
}

#[test]
fn test_creation_timestamp_ahead_of_device_clock_is_not_expiry() {
    let mut session = info();
    session.created_at += 60;
    assert!(is_unexpired(&session));
}

#[tokio::test]
async fn test_cancelled_observation_does_not_start_any_mutation_or_retry() {
    let original = info();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = server(Reply {
        status: StatusCode::OK,
        body: serde_json::to_vec(&original).unwrap(),
        owner: original.pubky.z32(),
        requests: Arc::clone(&requests),
        entered: Arc::clone(&entered),
        hold: Some(Arc::clone(&release)),
    })
    .await;
    let observation = tokio::spawn(async move {
        observer(original)
            .observe_request(reqwest::Client::new().get(url))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .expect("the observer should reach the controlled server");
    observation.abort();
    assert!(observation.await.unwrap_err().is_cancelled());
    release.notify_one();
    assert_eq!(
        *requests.lock().unwrap(),
        vec![(Method::GET, "/auth/grant/session".into())]
    );
    server.abort();
}

#[tokio::test]
async fn test_expired_or_unavailable_original_session_stays_unknown_and_redacted() {
    let mut expired = info();
    expired.token_expires_at = 0;
    let captured = observer(expired);
    assert_eq!(captured.observe().await, PubkyGrantObservation::Unknown);
    assert_eq!(format!("{captured:?}"), "PubkyGrantObserver(<redacted>)");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/auth/grant/session",
        listener.local_addr().unwrap()
    );
    drop(listener);
    assert_eq!(
        observer(info())
            .observe_request(reqwest::Client::new().get(url))
            .await,
        PubkyGrantObservation::Unknown
    );
}
