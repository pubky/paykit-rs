//! Approval of externally supplied Pubky signup auth URLs.
//!
//! The fixtures listen on loopback only and use fixed dummy keys. One server
//! plays both the PKARR relay and the auth relay inbox. The other is the
//! homeserver named by the auth URL; it answers every request with one fixed
//! status, as a requester-controlled homeserver could. Both record what they
//! receive, so each test asserts what approval sent and what it did not.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

use pubky::{
    pkarr::{dns::rdata::SVCB, Keypair, SignedPacket},
    PubkyHttpClient,
};

use super::*;

const IDENTITY_SECRET: [u8; 32] = [7; 32];
const PUBLISHED_HOMESERVER_SECRET: [u8; 32] = [0x11; 32];
const REQUESTED_HOMESERVER_SECRET: [u8; 32] = [0xAA; 32];
const CAPABILITIES: &str = PAYKIT_SESSION_CAPABILITIES;
const SIGNUP_REQUEST: &str = "POST /auth/grant/signup HTTP/1.1";
const CLAIM_PARAMETER: &str = "x-example-claim";
const CLAIM_TYPE: &str = "account-export-v1";

fn identity() -> PubkyLocalSecretKey {
    PubkyLocalSecretKey::new(IDENTITY_SECRET)
}

/// The identity's key as PKARR uses it to sign and address its record.
fn identity_record_signer() -> Keypair {
    Keypair::from_secret_key(&IDENTITY_SECRET)
}

/// Homeserver the identity's `_pubky` record points at before approval.
fn published_homeserver() -> Keypair {
    Keypair::from_secret_key(&PUBLISHED_HOMESERVER_SECRET)
}

/// Homeserver named by the `hs` parameter of the signup auth URL.
fn requested_homeserver() -> Keypair {
    Keypair::from_secret_key(&REQUESTED_HOMESERVER_SECRET)
}

fn z32(keypair: &Keypair) -> String {
    keypair.public_key().to_z32()
}

/// What one approval attempt did, as seen by the fixtures.
#[derive(Debug, PartialEq)]
struct Effects {
    /// Homeserver a fresh client resolves for the identity afterwards.
    homeserver: Option<String>,
    /// PKARR records published through the relay.
    records_published: usize,
    /// Messages delivered to the auth relay inbox.
    inbox_deliveries: usize,
    /// One entry for each connection the requested homeserver accepted.
    homeserver_requests: Vec<String>,
}

/// Ways the relay can fail to return a record it holds.
#[derive(Clone, Copy)]
enum LookupFault {
    /// Every lookup fails.
    Unavailable,
    /// Network lookups fail and a cache-only lookup finds nothing.
    NetworkUnavailable,
    /// The record aged out of the DHT. Only a cache-only lookup returns it.
    AgedOut,
    /// Network lookups find nothing and a cache-only lookup fails.
    CacheUnavailable,
}

/// State of the loopback PKARR relay and auth relay inbox.
#[derive(Default)]
struct Relay {
    /// PKARR relay payloads by z32 public key.
    records: Mutex<HashMap<String, Vec<u8>>>,
    /// Keys whose lookups do not simply return the stored record.
    lookup_faults: Mutex<HashMap<String, LookupFault>>,
    /// Method and path of every request, in arrival order.
    requests: Mutex<Vec<String>>,
}

impl Relay {
    fn put_record(&self, record: &SignedPacket) {
        self.records.lock().unwrap().insert(
            record.public_key().to_z32(),
            record.to_relay_payload().to_vec(),
        );
    }

    /// Serve `GET`/`PUT /<key>` as a PKARR relay and `POST /inbox/<channel>`
    /// as an auth relay.
    fn serve(&self, mut stream: TcpStream) {
        let Some((request_line, body)) = read_request(&mut stream) else {
            return;
        };
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let key = path.trim_start_matches('/');
        self.requests
            .lock()
            .unwrap()
            .push(format!("{method} {path}"));

        let record = self.records.lock().unwrap().get(key).cloned();
        match (method, self.lookup_failure(key, query), record) {
            ("GET", Some(status), _) => respond(&mut stream, status, &[]),
            ("GET", None, Some(record)) => respond(&mut stream, "200 OK", &record),
            ("PUT", ..) => {
                // A record that is published again resolves again.
                self.lookup_faults.lock().unwrap().remove(key);
                self.records.lock().unwrap().insert(key.to_owned(), body);
                respond(&mut stream, "204 No Content", &[]);
            }
            ("POST", ..) if key.starts_with("inbox/") => respond(&mut stream, "200 OK", &[]),
            _ => respond(&mut stream, "404 Not Found", &[]),
        }
    }

    /// Status that answers a lookup of `key` in place of its stored record.
    fn lookup_failure(&self, key: &str, query: &str) -> Option<&'static str> {
        let cache_only = query.split('&').any(|pair| pair == "policy=CacheOnly");
        match (self.lookup_faults.lock().unwrap().get(key)?, cache_only) {
            (LookupFault::Unavailable, _)
            | (LookupFault::NetworkUnavailable, false)
            | (LookupFault::CacheUnavailable, true) => Some("503 Service Unavailable"),
            (LookupFault::NetworkUnavailable, true)
            | (LookupFault::AgedOut, false)
            | (LookupFault::CacheUnavailable, false) => Some("404 Not Found"),
            (LookupFault::AgedOut, true) => None,
        }
    }
}

struct Fixture {
    relay_address: SocketAddr,
    relay: Arc<Relay>,
    homeserver_requests: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    /// Start the relay and the requested homeserver, which answers every
    /// request with `homeserver_status`.
    fn start(homeserver_status: &'static str) -> Self {
        let relay = Arc::new(Relay::default());
        let relay_address = {
            let relay = relay.clone();
            serve(move |stream| relay.serve(stream))
        };

        let homeserver = requested_homeserver();
        let homeserver_requests = Arc::new(Mutex::new(Vec::new()));
        let homeserver_address = {
            let tls = Arc::new(homeserver.to_rpk_rustls_server_config());
            let requests = homeserver_requests.clone();
            serve(move |stream| {
                serve_homeserver_request(stream, tls.clone(), homeserver_status, &requests);
            })
        };
        // Pubky clients find the homeserver's TLS endpoint through its record.
        let mut endpoint = SVCB::new(1, ".".try_into().unwrap());
        endpoint.set_port(homeserver_address.port());
        relay.put_record(
            &SignedPacket::builder()
                .https(".".try_into().unwrap(), endpoint, 3600)
                .address(".".try_into().unwrap(), Ipv4Addr::LOCALHOST.into(), 3600)
                .sign(&homeserver)
                .unwrap(),
        );

        Self {
            relay_address,
            relay,
            homeserver_requests,
        }
    }

    /// Publish the identity's `_pubky` record pointing at `homeserver`.
    fn publish_identity_record(&self, homeserver: &Keypair) {
        let target = z32(homeserver);
        self.relay.put_record(
            &SignedPacket::builder()
                .https(
                    "_pubky".try_into().unwrap(),
                    SVCB::new(0, target.as_str().try_into().unwrap()),
                    3600,
                )
                .sign(&identity_record_signer())
                .unwrap(),
        );
    }

    /// Choose how relay lookups of the identity's record fail, if at all.
    fn set_identity_lookup_fault(&self, fault: Option<LookupFault>) {
        let key = z32(&identity_record_signer());
        let mut faults = self.relay.lookup_faults.lock().unwrap();
        match fault {
            Some(fault) => faults.insert(key, fault),
            None => faults.remove(&key),
        };
    }

    /// A Pubky client whose only PKARR backend is the fixture relay.
    fn pubky(&self) -> Pubky {
        let relay = format!("http://{}", self.relay_address);
        let client = PubkyHttpClient::builder()
            .pkarr(|builder| {
                builder
                    .no_default_network()
                    .relays(&[relay.as_str()])
                    .unwrap()
            })
            .build()
            .unwrap();
        Pubky::with_client(client)
    }

    fn bootstrap(&self) -> PubkySessionBootstrap {
        PubkySessionBootstrap::with_pubky(self.pubky(), TEST_CLIENT_ID).unwrap()
    }

    fn signup_auth_url(&self) -> String {
        format!(
            "pubkyauth://signup_grant?caps={CAPABILITIES}&relay=http://{}/inbox/&secret={TEST_AUTH_SECRET}&hs={}&cid={TEST_CLIENT_ID}&cpk={TEST_PUBLIC_KEY}",
            self.relay_address,
            z32(&requested_homeserver()),
        )
    }

    fn signin_auth_url(&self) -> String {
        format!(
            "pubkyauth://signin_grant?caps={CAPABILITIES}&relay=http://{}/inbox/&secret={TEST_AUTH_SECRET}&cid={TEST_CLIENT_ID}&cpk={TEST_PUBLIC_KEY}",
            self.relay_address,
        )
    }

    async fn effects(&self) -> Effects {
        let homeserver = self
            .pubky()
            .get_homeserver_of(&identity().keypair().public_key())
            .await
            .unwrap()
            .map(|homeserver| homeserver.z32());
        let relay_requests = self.relay.requests.lock().unwrap();
        let count = |prefix: &str| {
            relay_requests
                .iter()
                .filter(|request| request.starts_with(prefix))
                .count()
        };
        Effects {
            homeserver,
            records_published: count("PUT /"),
            inbox_deliveries: count("POST /inbox/"),
            homeserver_requests: self.homeserver_requests.lock().unwrap().clone(),
        }
    }
}

/// Serve loopback connections on background threads until the test process
/// exits.
fn serve(handle: impl Fn(TcpStream) + Send + Sync + 'static) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = Arc::new(handle);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handle = handle.clone();
            std::thread::spawn(move || {
                // A silent connection must not park a fixture thread forever.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                handle(stream);
            });
        }
    });
    address
}

/// Record one connection to the requested homeserver and answer it with
/// `status`.
fn serve_homeserver_request(
    stream: TcpStream,
    tls: Arc<rustls::ServerConfig>,
    status: &str,
    requests: &Mutex<Vec<String>>,
) {
    // Logged before any I/O, so even a connection that never completes a
    // request shows up.
    let entry = {
        let mut requests = requests.lock().unwrap();
        requests.push("connection without a request".to_owned());
        requests.len() - 1
    };
    let Ok(connection) = rustls::ServerConnection::new(tls) else {
        return;
    };
    let mut stream = rustls::StreamOwned::new(connection, stream);
    if let Some((request_line, _)) = read_request(&mut stream) {
        requests.lock().unwrap()[entry] = request_line;
        respond(&mut stream, status, &[]);
        stream.conn.send_close_notify();
        let _ = stream.flush();
    }
}

/// Read one HTTP/1.1 request and return its request line and body.
fn read_request(stream: &mut impl Read) -> Option<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };
    let mut body = buffer.split_off(header_end);
    let head = String::from_utf8_lossy(&buffer).into_owned();
    let content_length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    Some((head.lines().next().unwrap_or_default().to_owned(), body))
}

fn respond(stream: &mut impl Write, status: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// Approve a signup auth URL naming a homeserver other than the one the
/// identity publishes. The requester controls that homeserver, so it chooses
/// `homeserver_status`.
async fn assert_different_homeserver_is_rejected(homeserver_status: &'static str) {
    let fixture = Fixture::start(homeserver_status);
    fixture.publish_identity_record(&published_homeserver());

    let result = fixture
        .bootstrap()
        .approve_auth(&fixture.signup_auth_url(), CAPABILITIES, &identity())
        .await;

    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: Some(z32(&published_homeserver())),
            records_published: 0,
            inbox_deliveries: 0,
            homeserver_requests: vec![],
        }
    );
    assert!(
        matches!(result, Err(PaykitSdkError::Policy { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn test_approve_auth_signup_grant_rejects_different_homeserver() {
    assert_different_homeserver_is_rejected("204 No Content").await;
}

#[tokio::test]
async fn test_approve_auth_signup_grant_rejects_different_homeserver_claiming_conflict() {
    assert_different_homeserver_is_rejected("409 Conflict").await;
}

#[tokio::test]
async fn test_approve_auth_signup_grant_approves_published_homeserver_without_republishing() {
    // A real homeserver answers a repeated signup with a conflict.
    let fixture = Fixture::start("409 Conflict");
    fixture.publish_identity_record(&requested_homeserver());

    let result = fixture
        .bootstrap()
        .approve_auth(&fixture.signup_auth_url(), CAPABILITIES, &identity())
        .await;

    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: Some(z32(&requested_homeserver())),
            records_published: 0,
            inbox_deliveries: 1,
            homeserver_requests: vec![],
        }
    );
    result.unwrap();
}

#[tokio::test]
async fn test_approve_auth_signup_grant_signs_up_identity_without_homeserver_record() {
    let fixture = Fixture::start("204 No Content");
    let auth_url = format!("{}&st=invite", fixture.signup_auth_url());

    let result = fixture
        .bootstrap()
        .approve_auth(&auth_url, CAPABILITIES, &identity())
        .await;

    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: Some(z32(&requested_homeserver())),
            records_published: 1,
            inbox_deliveries: 1,
            homeserver_requests: vec![
                "POST /auth/grant/signup?signup_token=invite HTTP/1.1".to_owned()
            ],
        }
    );
    result.unwrap();
}

#[tokio::test]
async fn test_approve_auth_signup_grant_does_not_publish_after_signup_conflict() {
    let fixture = Fixture::start("409 Conflict");

    let result = fixture
        .bootstrap()
        .approve_auth(&fixture.signup_auth_url(), CAPABILITIES, &identity())
        .await;

    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: None,
            records_published: 0,
            inbox_deliveries: 0,
            homeserver_requests: vec![SIGNUP_REQUEST.to_owned()],
        }
    );
    assert!(
        matches!(
            &result,
            Err(PaykitSdkError::Identity { context, .. }) if context.contains("sign_up")
        ),
        "{result:?}"
    );
}

/// Approve a signup auth URL while the relay does not simply return the
/// identity's record, which points at another homeserver.
async fn assert_unresolved_homeserver_record_is_not_replaced(fault: LookupFault) {
    let fixture = Fixture::start("204 No Content");
    fixture.publish_identity_record(&published_homeserver());
    fixture.set_identity_lookup_fault(Some(fault));

    let result = fixture
        .bootstrap()
        .approve_auth(&fixture.signup_auth_url(), CAPABILITIES, &identity())
        .await;

    fixture.set_identity_lookup_fault(None);
    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: Some(z32(&published_homeserver())),
            records_published: 0,
            inbox_deliveries: 0,
            homeserver_requests: vec![],
        }
    );
    assert!(
        matches!(result, Err(PaykitSdkError::Identity { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn test_approve_auth_signup_grant_fails_closed_when_homeserver_lookup_fails() {
    assert_unresolved_homeserver_record_is_not_replaced(LookupFault::NetworkUnavailable).await;
}

#[tokio::test]
async fn test_approve_auth_signup_grant_rejects_aged_out_homeserver_record() {
    assert_unresolved_homeserver_record_is_not_replaced(LookupFault::AgedOut).await;
}

#[tokio::test]
async fn test_approve_auth_signup_grant_fails_closed_when_record_cache_is_unreadable() {
    assert_unresolved_homeserver_record_is_not_replaced(LookupFault::CacheUnavailable).await;
}

#[tokio::test]
async fn test_approve_auth_signup_grant_approves_after_aged_out_record_is_republished() {
    let fixture = Fixture::start("409 Conflict");
    fixture.publish_identity_record(&requested_homeserver());
    fixture.set_identity_lookup_fault(Some(LookupFault::AgedOut));
    let auth_url = fixture.signup_auth_url();

    // Each step uses a fresh client, so none of them is served from a cache
    // that an earlier step filled.
    let refused = fixture
        .bootstrap()
        .approve_auth(&auth_url, CAPABILITIES, &identity())
        .await;
    let republished = fixture
        .bootstrap()
        .republish_identity(&identity().public_key())
        .await;
    let result = fixture
        .bootstrap()
        .approve_auth(&auth_url, CAPABILITIES, &identity())
        .await;

    assert!(
        matches!(refused, Err(PaykitSdkError::Identity { .. })),
        "{refused:?}"
    );
    assert!(republished.unwrap());
    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: Some(z32(&requested_homeserver())),
            // The rebroadcast of the identity's own record.
            records_published: 1,
            inbox_deliveries: 1,
            homeserver_requests: vec![],
        }
    );
    result.unwrap();
}

#[tokio::test]
async fn test_approve_auth_signin_grant_leaves_homeserver_record_unchanged() {
    let fixture = Fixture::start("204 No Content");
    fixture.publish_identity_record(&published_homeserver());
    // A sign-in approval must not depend on the homeserver lookup.
    fixture.set_identity_lookup_fault(Some(LookupFault::Unavailable));

    let result = fixture
        .bootstrap()
        .approve_auth(&fixture.signin_auth_url(), CAPABILITIES, &identity())
        .await;

    fixture.set_identity_lookup_fault(None);
    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: Some(z32(&published_homeserver())),
            records_published: 0,
            inbox_deliveries: 1,
            homeserver_requests: vec![],
        }
    );
    result.unwrap();
}

/// Approve the fixture's signup auth URL together with a companion claim.
async fn approve_signup_with_companion_claim(
    fixture: &Fixture,
) -> std::result::Result<(), PubkyAuthCompanionClaimApprovalError> {
    let auth_url = format!(
        "{}&{CLAIM_PARAMETER}={CLAIM_TYPE}",
        fixture.signup_auth_url()
    );
    let claim = PubkyAuthCompanionClaim::new(CLAIM_PARAMETER, CLAIM_TYPE, vec![1; 84]).unwrap();
    fixture
        .bootstrap()
        .approve_auth_with_companion_claim(&auth_url, CAPABILITIES, &identity(), &claim)
        .await
}

#[tokio::test]
async fn test_approve_auth_with_companion_claim_rejects_different_homeserver_before_delivery() {
    let fixture = Fixture::start("204 No Content");
    fixture.publish_identity_record(&published_homeserver());

    let result = approve_signup_with_companion_claim(&fixture).await;

    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: Some(z32(&published_homeserver())),
            records_published: 0,
            inbox_deliveries: 0,
            homeserver_requests: vec![],
        }
    );
    assert!(
        matches!(
            result,
            Err(PubkyAuthCompanionClaimApprovalError::InvalidAuthUrl { .. })
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn test_approve_auth_with_companion_claim_signs_up_identity_without_homeserver_record() {
    let fixture = Fixture::start("204 No Content");

    let result = approve_signup_with_companion_claim(&fixture).await;

    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: Some(z32(&requested_homeserver())),
            records_published: 1,
            // The companion claim, then the grant.
            inbox_deliveries: 2,
            homeserver_requests: vec![SIGNUP_REQUEST.to_owned()],
        }
    );
    result.unwrap();
}

#[tokio::test]
async fn test_approve_auth_with_companion_claim_fails_closed_before_delivery_when_lookup_fails() {
    let fixture = Fixture::start("204 No Content");
    fixture.publish_identity_record(&published_homeserver());
    fixture.set_identity_lookup_fault(Some(LookupFault::Unavailable));

    let result = approve_signup_with_companion_claim(&fixture).await;

    fixture.set_identity_lookup_fault(None);
    assert_eq!(
        fixture.effects().await,
        Effects {
            homeserver: Some(z32(&published_homeserver())),
            records_published: 0,
            inbox_deliveries: 0,
            homeserver_requests: vec![],
        }
    );
    assert!(
        matches!(
            result,
            Err(PubkyAuthCompanionClaimApprovalError::InvalidAuthUrl { .. })
        ),
        "{result:?}"
    );
}
