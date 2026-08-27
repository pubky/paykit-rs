use std::time::Duration;

use crypto_secretbox::{aead::Aead, KeyInit, XSalsa20Poly1305};
use pubky::{
    Capabilities, EncryptedHttpRelayInboxChannel, GrantClaims, HttpRelayInboxChannel, Keypair,
    Pubky, PubkyHttpClient,
};

use super::*;

const QUERY_PARAMETER: &str = "x-example-claim";
const CLAIM_TYPE: &str = "account-export-v1";
const CAPABILITY: &str = "/pub/example/account/:rw";
const CLIENT_ID: &str = "paykit.test";
const CLIENT_KEY_SECRET: [u8; 32] = [11; 32];

fn test_claim() -> PubkyAuthCompanionClaim {
    PubkyAuthCompanionClaim::new(QUERY_PARAMETER, CLAIM_TYPE, (0_u8..84).collect()).unwrap()
}

#[test]
fn test_companion_claim_debug_redacts_unsigned_payload() {
    let claim = PubkyAuthCompanionClaim::new(QUERY_PARAMETER, CLAIM_TYPE, vec![222, 173, 190, 239])
        .unwrap();

    let debug = format!("{claim:?}");

    assert!(debug.contains(QUERY_PARAMETER));
    assert!(debug.contains(CLAIM_TYPE));
    assert!(debug.contains("<redacted:4 bytes>"));
    assert!(!debug.contains("[222, 173, 190, 239]"));
}

fn auth_url(relay: &Url, secret: &[u8; 32], claim_type: &str) -> String {
    auth_url_for_client_id(relay, secret, claim_type, CLIENT_ID)
}

fn auth_url_for_client_id(
    relay: &Url,
    secret: &[u8; 32],
    claim_type: &str,
    client_id: &str,
) -> String {
    let client_public_key = Keypair::from_secret(&CLIENT_KEY_SECRET).public_key();
    format!(
        "pubkyauth://signin_grant?caps={CAPABILITY}&relay={relay}&secret={}&cid={client_id}&cpk={}&{QUERY_PARAMETER}={claim_type}",
        URL_SAFE_NO_PAD.encode(secret),
        client_public_key.z32(),
    )
}

fn decrypt_claim(ciphertext: &[u8], secret: &[u8; 32]) -> Vec<u8> {
    let cipher = XSalsa20Poly1305::new(secret.into());
    cipher
        .decrypt((&ciphertext[..24]).into(), &ciphertext[24..])
        .unwrap()
}

#[test]
fn test_companion_claim_signing_uses_integrator_protocol_identifiers() {
    let claim = test_claim();
    let identity = PubkyLocalSecretKey::new([7; 32]);
    let auth_secret = [3; 32];

    let payload = encode_signed_claim(&claim, &auth_secret, &identity);

    assert_eq!(payload.len(), 148);
    assert_eq!(&payload[..84], &(0_u8..84).collect::<Vec<_>>());

    let mut signable = format!("{QUERY_PARAMETER}|{CLAIM_TYPE}|").into_bytes();
    signable.extend_from_slice(&Sha256::digest(auth_secret));
    signable.extend_from_slice(&payload[..84]);
    let expected_signature = identity.keypair().sign(&signable).to_bytes();
    assert_eq!(&payload[84..], &expected_signature);
}

#[test]
fn test_companion_claim_rejects_invalid_protocol_identifiers() {
    assert!(matches!(
        PubkyAuthCompanionClaim::new("x-example|claim", CLAIM_TYPE, vec![]),
        Err(PubkyAuthCompanionClaimApprovalError::InvalidClaim { .. })
    ));
    assert!(matches!(
        PubkyAuthCompanionClaim::new(QUERY_PARAMETER, "", vec![]),
        Err(PubkyAuthCompanionClaimApprovalError::InvalidClaim { .. })
    ));
}

#[test]
fn test_companion_claim_rejects_auth_claim_type_mismatch() {
    let relay = Url::parse("https://relay.example/inbox").unwrap();
    let invalid_url = auth_url(&relay, &[3; 32], "different-claim-v1");
    assert!(matches!(
        parse_companion_auth_request(&invalid_url, CAPABILITY, &test_claim()),
        Err(PubkyAuthCompanionClaimApprovalError::InvalidAuthUrl { .. })
    ));
}

#[test]
fn test_companion_query_uses_url_decoding() {
    let url = Url::parse("pubkyauth://signin?x-example-claim=account%2Dexport%2Dv1").unwrap();

    assert_eq!(
        unique_query_value(&url, QUERY_PARAMETER).unwrap(),
        CLAIM_TYPE
    );
}

#[test]
fn test_companion_auth_request_parses_relay_and_secret() {
    let relay = Url::parse("https://relay.example/inbox").unwrap();
    let secret = [3; 32];
    let url = auth_url(&relay, &secret, CLAIM_TYPE);

    let request = parse_companion_auth_request(&url, CAPABILITY, &test_claim()).unwrap();

    assert_eq!(request.relay, relay);
    assert_eq!(request.secret, secret);
}

#[test]
fn test_companion_auth_request_rejects_duplicate_secret() {
    let relay = Url::parse("https://relay.example/inbox").unwrap();
    let secret = [3; 32];
    let url = format!(
        "{}&secret={}",
        auth_url(&relay, &secret, CLAIM_TYPE),
        URL_SAFE_NO_PAD.encode(secret)
    );

    assert!(matches!(
        parse_companion_auth_request(&url, CAPABILITY, &test_claim()),
        Err(PubkyAuthCompanionClaimApprovalError::InvalidAuthUrl { .. })
    ));
}

#[test]
fn test_companion_channel_uses_claim_type_and_decoded_secret() {
    let auth_secret = [9; 32];
    let mut channel_input = CLAIM_TYPE.as_bytes().to_vec();
    channel_input.push(b'|');
    channel_input.extend_from_slice(&auth_secret);
    let expected = URL_SAFE_NO_PAD.encode(blake3::hash(&channel_input).as_bytes());

    assert_eq!(
        derive_companion_channel_id(CLAIM_TYPE, &auth_secret),
        expected
    );
}

#[test]
fn test_relay_delivery_failure_redacts_server_details() {
    let error = relay_delivery_failure(PubkyError::Request(RequestError::Server {
        status: "503".parse().unwrap(),
        message: "https://relay.example/inbox/sensitive-channel: secret body".into(),
    }));

    let reason = match &error {
        PubkyAuthCompanionClaimApprovalError::RelayDeliveryFailure { reason } => reason,
        other => panic!("expected relay delivery failure, got {other:?}"),
    };
    assert_eq!(reason, "relay returned HTTP status 503");
    assert!(!error.to_string().contains("sensitive-channel"));
    assert!(!error.to_string().contains("secret body"));
}

#[tokio::test]
async fn test_approve_auth_with_companion_claim_delivers_both_envelopes() {
    let relay = http_relay::HttpRelay::builder()
        .http_port(0)
        .run()
        .await
        .unwrap();
    let inbox = relay.local_url().join("inbox").unwrap();
    let client = PubkyHttpClient::new().unwrap();
    let bootstrap =
        PubkySessionBootstrap::with_pubky(Pubky::with_client(client.clone()), CLIENT_ID).unwrap();
    let auth_secret = [9; 32];
    let identity = PubkyLocalSecretKey::new([7; 32]);
    let auth_url = auth_url(&inbox, &auth_secret, CLAIM_TYPE);

    bootstrap
        .approve_auth_with_companion_claim(&auth_url, CAPABILITY, &identity, &test_claim())
        .await
        .unwrap();

    let claim_channel = HttpRelayInboxChannel::new(
        inbox.clone(),
        derive_companion_channel_id(CLAIM_TYPE, &auth_secret),
    )
    .unwrap();
    let encrypted_claim = claim_channel
        .poll(&client, Some(Duration::from_secs(1)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(encrypted_claim.len(), 24 + 148 + 16);
    let claim_payload = decrypt_claim(&encrypted_claim, &auth_secret);
    assert_eq!(claim_payload.len(), 148);

    let auth_channel = EncryptedHttpRelayInboxChannel::new(inbox, auth_secret).unwrap();
    let grant_bytes = auth_channel
        .poll(&client, Some(Duration::from_secs(1)))
        .await
        .unwrap()
        .unwrap();
    let grant = GrantClaims::decode(std::str::from_utf8(&grant_bytes).unwrap()).unwrap();
    assert_eq!(grant.iss, identity.keypair().public_key());
    assert_eq!(grant.client_id.as_str(), CLIENT_ID);
    assert_eq!(Capabilities::from(grant.caps).to_string(), CAPABILITY);
    assert_eq!(
        grant.cnf,
        Keypair::from_secret(&CLIENT_KEY_SECRET).public_key()
    );
}

#[tokio::test]
async fn test_approve_auth_with_companion_claim_rejects_mismatched_client_before_delivery() {
    let relay = http_relay::HttpRelay::builder()
        .http_port(0)
        .run()
        .await
        .unwrap();
    let inbox = relay.local_url().join("inbox").unwrap();
    let client = PubkyHttpClient::new().unwrap();
    let bootstrap =
        PubkySessionBootstrap::with_pubky(Pubky::with_client(client.clone()), CLIENT_ID).unwrap();
    let auth_secret = [9; 32];
    let auth_url = auth_url_for_client_id(&inbox, &auth_secret, CLAIM_TYPE, "attacker.test");

    let error = bootstrap
        .approve_auth_with_companion_claim(
            &auth_url,
            CAPABILITY,
            &PubkyLocalSecretKey::new([7; 32]),
            &test_claim(),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        PubkyAuthCompanionClaimApprovalError::InvalidAuthUrl { .. }
    ));

    let claim_channel = HttpRelayInboxChannel::new(
        inbox.clone(),
        derive_companion_channel_id(CLAIM_TYPE, &auth_secret),
    )
    .unwrap();
    assert!(claim_channel
        .poll(&client, Some(Duration::from_millis(50)))
        .await
        .unwrap()
        .is_none());

    let auth_channel = EncryptedHttpRelayInboxChannel::new(inbox, auth_secret).unwrap();
    assert!(auth_channel
        .poll(&client, Some(Duration::from_millis(50)))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_approve_auth_with_companion_claim_reports_relay_failure() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = Url::parse(&format!("http://{}/inbox", listener.local_addr().unwrap())).unwrap();
    drop(listener);
    let bootstrap = PubkySessionBootstrap::new(CLIENT_ID).unwrap();
    let auth_secret = [9; 32];
    let auth_url = auth_url(&relay, &auth_secret, CLAIM_TYPE);
    let channel_id = derive_companion_channel_id(CLAIM_TYPE, &auth_secret);

    let error = bootstrap
        .approve_auth_with_companion_claim(
            &auth_url,
            CAPABILITY,
            &PubkyLocalSecretKey::new([7; 32]),
            &test_claim(),
        )
        .await
        .unwrap_err();

    let reason = match &error {
        PubkyAuthCompanionClaimApprovalError::RelayDeliveryFailure { reason } => reason,
        other => panic!("expected relay delivery failure, got {other:?}"),
    };
    assert_eq!(reason, "relay HTTP transport failed");

    let display = error.to_string();
    assert!(!display.contains(relay.as_str()));
    assert!(!display.contains(&channel_id));
    assert!(!display.contains(&URL_SAFE_NO_PAD.encode(auth_secret)));
}
