use std::collections::HashMap;

use crate::*;
use pubky::PubkySession;
use pubky_testnet::{embedded_postgres::EmbeddedPostgres, pubky::Keypair, EphemeralTestnet};
use tokio::sync::{Mutex as TokioMutex, OnceCell};

mod encrypted_link;
mod event_id;
mod payment_endpoint;
mod payment_request;
mod payment_request_properties;
mod private_payment_list;
mod receipt_access;

static SHARED_POSTGRES: OnceCell<EmbeddedPostgres> = OnceCell::const_new();
static TESTNET_BUILD_LOCK: TokioMutex<()> = TokioMutex::const_new(());

async fn shared_postgres() -> &'static EmbeddedPostgres {
    SHARED_POSTGRES
        .get_or_init(|| async {
            EmbeddedPostgres::start()
                .await
                .expect("failed to start embedded postgres")
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
            .expect("embedded postgres connection string should be valid");
        EphemeralTestnet::builder().postgres(postgres)
    };

    builder.build().await.unwrap()
}

struct TestSetup {
    _testnet: EphemeralTestnet,
    session: PubkySession,
    public_storage: pubky::PublicStorage,
    raw_session: PubkySession,
    public_key: PublicKey,
}

fn receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

impl TestSetup {
    async fn new() -> Self {
        let testnet = build_testnet().await;

        let homeserver = testnet.homeserver_app();
        let sdk = testnet.sdk().unwrap();

        let pair = Keypair::random();
        let signer = sdk.signer(pair.clone());
        let session = signer.signup(&homeserver.public_key(), None).await.unwrap();

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
        let initiator_signer = initiator_sdk.signer(initiator_keypair.clone());
        let initiator_session = initiator_signer
            .signup(&homeserver.public_key(), None)
            .await
            .unwrap();

        let responder_keypair = Keypair::random();
        let responder_signer = responder_sdk.signer(responder_keypair.clone());
        let responder_session = responder_signer
            .signup(&homeserver.public_key(), None)
            .await
            .unwrap();

        let initiator_public_key = initiator_session.info().public_key();
        let responder_public_key = responder_session.info().public_key();
        let initiator_noise_keypair = Keypair::random();
        let responder_noise_keypair = Keypair::random();

        let initiator_handshake = initiate_encrypted_link(
            initiator_session.clone(),
            initiator_noise_keypair.secret_key(),
            responder_public_key,
            &responder_noise_keypair.public_key(),
            &receiver_path(),
            &receiver_path(),
            initiator_sdk,
        )
        .unwrap();

        let responder_handshake = accept_encrypted_link(
            responder_session.clone(),
            responder_noise_keypair.secret_key(),
            initiator_public_key,
            &initiator_noise_keypair.public_key(),
            &receiver_path(),
            &receiver_path(),
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
        let sender_signer = sender_sdk.signer(sender_keypair.clone());
        let sender_session = sender_signer
            .signup(&homeserver.public_key(), None)
            .await
            .unwrap();

        let receiver_keypair = Keypair::random();
        let receiver_signer = receiver_sdk.signer(receiver_keypair.clone());
        let receiver_session = receiver_signer
            .signup(&homeserver.public_key(), None)
            .await
            .unwrap();

        let sender_public_key = sender_session.info().public_key();
        let receiver_public_key = receiver_session.info().public_key();
        let sender_noise_keypair = Keypair::random();
        let receiver_noise_keypair = Keypair::random();

        // Initiate handshake from sender side.
        let sender_handshake = initiate_encrypted_link(
            sender_session.clone(),
            sender_noise_keypair.secret_key(),
            receiver_public_key,
            &receiver_noise_keypair.public_key(),
            &receiver_path(),
            &receiver_path(),
            sender_sdk,
        )
        .unwrap();

        // Accept handshake from receiver side.
        let receiver_handshake = accept_encrypted_link(
            receiver_session.clone(),
            receiver_noise_keypair.secret_key(),
            sender_public_key,
            &sender_noise_keypair.public_key(),
            &receiver_path(),
            &receiver_path(),
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

fn private_payment_list(
    payment_endpoints: &HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
) -> PrivatePaymentList {
    PrivatePaymentList::new(payment_endpoints.clone())
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
