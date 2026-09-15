use std::collections::HashMap;

use crate::*;
use pubky::{ClientId, Pubky, PubkySession, PublicKey};
use pubky_testnet::{docker_postgres::DockerPostgres, pubky::Keypair, EphemeralTestnet};
use tokio::sync::{Mutex as TokioMutex, OnceCell};

mod encrypted_link;
mod event_id;
mod payment_endpoint;
mod payment_request;
mod payment_request_properties;
mod private_payment_list;
mod receipt_access;
mod routing_tracing;

const TEST_CLIENT_ID: &str = "paykit-lib.test";

static SHARED_POSTGRES: OnceCell<DockerPostgres> = OnceCell::const_new();
static TESTNET_BUILD_LOCK: TokioMutex<()> = TokioMutex::const_new(());

async fn shared_postgres() -> &'static DockerPostgres {
    SHARED_POSTGRES
        .get_or_init(|| async {
            DockerPostgres::start()
                .await
                .expect("failed to start Docker Postgres")
        })
        .await
}

async fn build_testnet() -> EphemeralTestnet {
    let _guard = TESTNET_BUILD_LOCK.lock().await;

    let builder = if std::env::var_os("TEST_PUBKY_CONNECTION_STRING").is_some() {
        EphemeralTestnet::builder()
    } else {
        let postgres = shared_postgres()
            .await
            .connection_string()
            .expect("Docker Postgres connection string should be valid");
        EphemeralTestnet::builder().postgres(postgres)
    };

    builder.build().await.unwrap()
}

async fn signup_session(sdk: &Pubky, homeserver: &PublicKey, keypair: &Keypair) -> PubkySession {
    let signer = sdk.signer(keypair.clone());
    signer.signup(homeserver, None).await.unwrap();
    signer
        .signin(ClientId::new(TEST_CLIENT_ID).unwrap())
        .await
        .unwrap()
}

struct TestSetup {
    _testnet: EphemeralTestnet,
    session: PubkySession,
    public_storage: pubky::PublicStorage,
    raw_session: PubkySession,
    public_key: PublicKey,
}

impl TestSetup {
    async fn new() -> Self {
        let testnet = build_testnet().await;

        let homeserver = testnet.homeserver_app();
        let sdk = testnet.sdk().unwrap();

        let pair = Keypair::random();
        let session = signup_session(&sdk, &homeserver.public_key(), &pair).await;

        let public_storage = sdk.public_storage();

        Self {
            _testnet: testnet,
            session: session.clone(),
            public_storage,
            raw_session: session,
            public_key: pair.public_key(),
        }
    }
}

/// Test setup that creates two users and initializes handshake handles
/// without driving them to completion.
struct InProgressHandshakeSetup {
    _testnet: EphemeralTestnet,
    initiator_session: PubkySession,
    responder_session: PubkySession,
    initiator_handshake: EncryptedLinkHandshake,
    responder_handshake: EncryptedLinkHandshake,
}

impl InProgressHandshakeSetup {
    async fn new() -> Self {
        let testnet = build_testnet().await;
        let homeserver = testnet.homeserver_app();

        let initiator_sdk = testnet.sdk().unwrap();
        let responder_sdk = testnet.sdk().unwrap();

        let initiator_keypair = Keypair::random();
        let initiator_session =
            signup_session(&initiator_sdk, &homeserver.public_key(), &initiator_keypair).await;

        let responder_keypair = Keypair::random();
        let responder_session =
            signup_session(&responder_sdk, &homeserver.public_key(), &responder_keypair).await;

        let initiator_info = initiator_session.info();
        let responder_info = responder_session.info();
        let initiator_public_key = initiator_info.public_key();
        let responder_public_key = responder_info.public_key();
        let initiator_noise_secret_key =
            derive_paykit_noise_secret_key(&initiator_keypair.secret_key());
        let responder_noise_secret_key =
            derive_paykit_noise_secret_key(&responder_keypair.secret_key());
        let initiator_noise_public_key =
            Keypair::from_secret(&initiator_noise_secret_key).public_key();
        let responder_noise_public_key =
            Keypair::from_secret(&responder_noise_secret_key).public_key();

        let initiator_handshake = initiate_encrypted_link(
            initiator_session.clone(),
            initiator_noise_secret_key,
            responder_public_key,
            &responder_noise_public_key,
            initiator_sdk,
        )
        .unwrap();

        let responder_handshake = accept_encrypted_link(
            responder_session.clone(),
            responder_noise_secret_key,
            initiator_public_key,
            &initiator_noise_public_key,
            responder_sdk,
        )
        .unwrap();

        Self {
            _testnet: testnet,
            initiator_session,
            responder_session,
            initiator_handshake,
            responder_handshake,
        }
    }
}

/// Test setup for private (encrypted) payment flows.
///
/// Creates two users on the same ephemeral testnet, performs a full Noise XX
/// handshake between them using the public `initiate_encrypted_link` /
/// `accept_encrypted_link` / `advance_handshake` API, and produces ready-to-use
/// [`EncryptedLink`] handles so Private Application Messages can be exchanged.
struct PrivateTestSetup {
    _testnet: EphemeralTestnet,
    /// Sender's Encrypted Link (writes Private Payment Lists).
    sender_link: EncryptedLink,
    /// Sender's session (kept for cleanup via `signout`).
    sender_session: PubkySession,
    /// Receiver's Encrypted Link (reads Private Payment Lists).
    receiver_link: EncryptedLink,
    /// Receiver's session (kept for cleanup via `signout`).
    receiver_session: PubkySession,
}

/// Drives a handshake to completion by polling `advance_handshake` with a
/// short sleep between retries. Panics on timeout (10 s).
async fn drive_handshake_to_completion(mut handshake: EncryptedLinkHandshake) -> EncryptedLink {
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let timeout = Duration::from_secs(10);

    loop {
        assert!(
            start.elapsed() < timeout,
            "handshake timed out after {timeout:?}"
        );

        match advance_handshake(handshake).await.unwrap() {
            HandshakeProgress::Pending(h) => {
                handshake = h;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            HandshakeProgress::Complete(link) => return link,
        }
    }
}

impl PrivateTestSetup {
    async fn new() -> Self {
        let testnet = build_testnet().await;
        let homeserver = testnet.homeserver_app();

        // Each user gets its own Pubky SDK instance.
        let sender_sdk = testnet.sdk().unwrap();
        let receiver_sdk = testnet.sdk().unwrap();

        // Sign up two independent users.
        let sender_keypair = Keypair::random();
        let sender_session =
            signup_session(&sender_sdk, &homeserver.public_key(), &sender_keypair).await;

        let receiver_keypair = Keypair::random();
        let receiver_session =
            signup_session(&receiver_sdk, &homeserver.public_key(), &receiver_keypair).await;

        let sender_info = sender_session.info();
        let receiver_info = receiver_session.info();
        let sender_public_key = sender_info.public_key();
        let receiver_public_key = receiver_info.public_key();
        let sender_noise_secret_key = derive_paykit_noise_secret_key(&sender_keypair.secret_key());
        let receiver_noise_secret_key =
            derive_paykit_noise_secret_key(&receiver_keypair.secret_key());
        let sender_noise_public_key = Keypair::from_secret(&sender_noise_secret_key).public_key();
        let receiver_noise_public_key =
            Keypair::from_secret(&receiver_noise_secret_key).public_key();

        // Initiate handshake from sender side.
        let sender_handshake = initiate_encrypted_link(
            sender_session.clone(),
            sender_noise_secret_key,
            receiver_public_key,
            &receiver_noise_public_key,
            sender_sdk,
        )
        .unwrap();

        // Accept handshake from receiver side.
        let receiver_handshake = accept_encrypted_link(
            receiver_session.clone(),
            receiver_noise_secret_key,
            sender_public_key,
            &sender_noise_public_key,
            receiver_sdk,
        )
        .unwrap();

        // Drive both handshakes to completion concurrently.
        let (sender_link, receiver_link) = tokio::join!(
            drive_handshake_to_completion(sender_handshake),
            drive_handshake_to_completion(receiver_handshake),
        );

        Self {
            _testnet: testnet,
            sender_link,
            sender_session,
            receiver_link,
            receiver_session,
        }
    }
}

// Shared helpers for integration-style tests under this module.

fn test_app_id() -> PaykitAppId {
    PaykitAppId::new("test-app").unwrap()
}

fn private_payment_list(
    payment_endpoints: &HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
) -> PrivatePaymentList {
    PrivatePaymentList::new(test_app_id(), payment_endpoints.clone())
}

async fn send_raw_private_application_message(link: &mut EncryptedLink, json: &str) {
    assert!(
        json.len() <= pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
        "test Private Application Message exceeds pubky-noise message size"
    );
    link.send_private_application_message_for_test(json.as_bytes())
        .await
        .expect("raw Private Application Message should send");
}

async fn receive_latest_private_payment_list_for_test(
    link: &mut EncryptedLink,
) -> Option<PrivatePaymentList> {
    link.receive_private_application_messages()
        .await
        .expect("Private Application Message stream receive should succeed")
        .into_iter()
        .filter(|message| message.known_kind() == Some(PrivateMessageKind::PrivatePaymentList))
        .filter_map(|message| parse_private_payment_list_json(&message.raw_json).ok())
        .next_back()
}

async fn receive_receipt_access_for_test(link: &mut EncryptedLink) -> Vec<ReceiptAccess> {
    link.receive_private_application_messages()
        .await
        .expect("Private Application Message stream receive should succeed")
        .iter()
        .filter_map(parse_receipt_access_event_message)
        .filter_map(|message| message.access.ok())
        .collect()
}

async fn receive_payment_request_events_for_test(
    link: &mut EncryptedLink,
) -> Vec<PaymentRequestEventMessage> {
    link.receive_private_application_messages()
        .await
        .expect("Private Application Message stream receive should succeed")
        .iter()
        .filter_map(parse_payment_request_event_message)
        .collect()
}
