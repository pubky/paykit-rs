use std::{
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};

use pubky::pkarr::{dns::rdata::SVCB, Cache, InMemoryCache, SignedPacket};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use tokio_rustls::TlsAcceptor;

use super::*;

struct SentProof {
    value: serde_json::Value,
    response: oneshot::Sender<Option<Vec<u8>>>,
}

struct Fixture {
    logout: Arc<PubkyGrantLogout>,
    proofs: mpsc::UnboundedReceiver<SentProof>,
    gets: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn response(status: u16, body: &[u8], headers: &str) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

impl Fixture {
    async fn new(info_response: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let homeserver = Keypair::random();
        let acceptor = TlsAcceptor::from(Arc::new(homeserver.to_rpk_rustls_server_config()));
        let (tx, proofs) = mpsc::unbounded_channel();
        let gets = Arc::new(AtomicUsize::new(0));
        let get_count = Arc::clone(&gets);
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let Ok(mut stream) = acceptor.accept(socket).await else {
                    continue;
                };
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let (header_end, length) = loop {
                    let size = stream.read(&mut buffer).await.unwrap();
                    if size == 0 {
                        break (0, 0);
                    }
                    bytes.extend_from_slice(&buffer[..size]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length: usize = header
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .unwrap_or("0")
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break (end, length);
                        }
                    }
                };
                if header_end == 0 {
                    continue;
                }
                let header = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
                assert!(!header.contains("authorization:") && !header.contains("cookie:"));
                if header.starts_with("get /info ") {
                    get_count.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(&info_response).await;
                } else {
                    assert!(header.starts_with("delete /auth/grant/session "));
                    assert!(header.contains("content-type: application/json"));
                    let value =
                        serde_json::from_slice(&bytes[header_end + 4..header_end + 4 + length])
                            .unwrap();
                    let (response, rx) = oneshot::channel();
                    tx.send(SentProof { value, response })
                        .unwrap_or_else(|_| panic!("proof receiver closed"));
                    if let Ok(Some(bytes)) = rx.await {
                        let _ = stream.write_all(&bytes).await;
                    }
                }
            }
        });
        let cache: Arc<dyn Cache> = Arc::new(InMemoryCache::new(NonZeroUsize::new(1).unwrap()));
        let mut endpoint = SVCB::new(0, ".".try_into().unwrap());
        endpoint.set_port(address.port());
        let packet = SignedPacket::builder()
            .https(".".try_into().unwrap(), endpoint, 3600)
            .address(".".try_into().unwrap(), address.ip(), 3600)
            .sign(&homeserver)
            .unwrap();
        cache.put(&homeserver.public_key().into_inner().into(), &packet);
        let client = pubky::PubkyHttpClient::builder()
            .pkarr(|b| {
                b.no_default_network()
                    .cache(cache)
                    .relays(&["http://127.0.0.1:1"])
                    .unwrap()
            })
            .build()
            .unwrap();
        let (_, secret) = original_at(homeserver.public_key());
        let session = PubkySession::from_grant_credential(
            client,
            pubky::GrantCredential::from_shared_secret(&secret).unwrap(),
        );
        let logout = Arc::new(
            PubkyGrantLogout::capture(&session, Duration::from_secs(2))
                .await
                .unwrap(),
        );
        Self {
            logout,
            proofs,
            gets,
            task,
        }
    }

    async fn supported() -> Self {
        Self::new(response(200, br#"{"features":["grant-proof-logout"]}"#, "")).await
    }

    async fn proof(&mut self) -> SentProof {
        tokio::time::timeout(Duration::from_secs(3), self.proofs.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

fn claims(proof: &SentProof) -> PopProofClaims {
    pubky_common::auth::jws::decode_jws_payload(proof.value["pop"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn test_lost_response_explicit_retry_uses_same_grant_fresh_nonce_and_retains_ack() {
    let mut fixture = Fixture::supported().await;
    let first = fixture.logout.prepare_attempt().await.unwrap();
    assert!(
        first.request.body().is_none(),
        "proof must be signed at execution"
    );
    let run = tokio::spawn(first.execute());
    let sent = fixture.proof().await;
    let first_claims = claims(&sent);
    let grant = sent.value["grant"].as_str().unwrap().to_owned();
    sent.response.send(None).unwrap();
    assert!(run.await.unwrap().is_err());
    assert!(fixture.logout.acknowledged().is_none());
    assert!(fixture.proofs.try_recv().is_err(), "no automatic replay");

    let retry = fixture.logout.prepare_attempt().await.unwrap();
    let run = tokio::spawn(retry.execute());
    let sent = fixture.proof().await;
    let next_claims = claims(&sent);
    assert_ne!(first_claims.nonce, next_claims.nonce);
    assert_eq!(first_claims.gid, next_claims.gid);
    assert!(sent.value["grant"].as_str().unwrap() == grant);
    sent.response
        .send(Some(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n".to_vec(),
        ))
        .unwrap();
    let receipt = run.await.unwrap().unwrap();
    assert_eq!(fixture.logout.acknowledged(), Some(receipt));
    assert!(fixture.logout.prepare_attempt().await.is_err());
    assert_eq!(fixture.gets.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_cancellation_before_dispatch_and_after_dispatch_preserves_unknown() {
    let mut fixture = Fixture::supported().await;
    drop(fixture.logout.prepare_attempt().await.unwrap());
    assert!(fixture.proofs.try_recv().is_err());
    let run = tokio::spawn(fixture.logout.prepare_attempt().await.unwrap().execute());
    let sent = fixture.proof().await;
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
    sent.response.send(None).unwrap();
    assert!(fixture.logout.acknowledged().is_none());
    let retry = fixture.logout.prepare_attempt().await.unwrap();
    drop(retry);
    assert!(fixture.proofs.try_recv().is_err());
}

#[tokio::test]
async fn test_support_and_redirect_failures_never_dispatch_or_replay() {
    for info in [
        response(200, b"{}", ""),
        response(200, br#"{"features":[]}"#, ""),
        response(503, b"", ""),
        response(200, &vec![b' '; MAX_INFO_BYTES + 1], ""),
        response(307, b"", "Location: https://example.invalid/forbidden\r\n"),
    ] {
        let fixture = Fixture::new(info).await;
        assert!(fixture.logout.prepare_attempt().await.is_err());
        assert!(fixture.logout.acknowledged().is_none());
        assert_eq!(fixture.gets.load(Ordering::SeqCst), 1);
    }
    for status in [307, 401, 500] {
        let mut fixture = Fixture::supported().await;
        let run = tokio::spawn(fixture.logout.prepare_attempt().await.unwrap().execute());
        fixture
            .proof()
            .await
            .response
            .send(Some(response(
                status,
                b"",
                "Location: https://example.invalid/forbidden\r\n",
            )))
            .unwrap();
        assert!(run.await.unwrap().is_err());
        assert!(fixture.logout.acknowledged().is_none());
        assert!(fixture.proofs.try_recv().is_err());
    }
}
