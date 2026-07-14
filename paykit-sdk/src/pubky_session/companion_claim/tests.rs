use std::time::Duration;

use crypto_secretbox::{aead::Aead, KeyInit, XSalsa20Poly1305};
use pubky::{
    AuthToken, EncryptedHttpRelayInboxChannel, HttpRelayInboxChannel, Pubky, PubkyHttpClient,
};

use super::*;

const QUERY_PARAMETER: &str = "x-example-claim";
const CLAIM_TYPE: &str = "account-export-v1";
const CAPABILITY: &str = "/pub/example/account/:rw";

fn test_claim() -> PubkyAuthCompanionClaim {
    PubkyAuthCompanionClaim::new(QUERY_PARAMETER, CLAIM_TYPE, (0_u8..84).collect()).unwrap()
}

fn auth_url(relay: &Url, secret: &[u8; 32], claim_type: &str) -> String {
    format!(
        "pubkyauth://signin?caps={CAPABILITY}&relay={relay}&secret={}&{QUERY_PARAMETER}={claim_type}",
        URL_SAFE_NO_PAD.encode(secret)
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
fn test_companion_query_decoding_preserves_literal_plus() {
    let url = Url::parse("pubkyauth://signin?secret=one%2Ftwo+three").unwrap();

    assert_eq!(unique_query_value(&url, "secret").unwrap(), "one/two+three");
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

#[tokio::test]
async fn test_approve_auth_with_companion_claim_delivers_both_envelopes() {
    let relay = http_relay::HttpRelay::builder()
        .http_port(0)
        .run()
        .await
        .unwrap();
    let inbox = relay.local_url().join("inbox").unwrap();
    let client = PubkyHttpClient::new().unwrap();
    let bootstrap = PubkySessionBootstrap::with_pubky(Pubky::with_client(client.clone()));
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
    let token_bytes = auth_channel
        .poll(&client, Some(Duration::from_secs(1)))
        .await
        .unwrap()
        .unwrap();
    let token = AuthToken::verify(&token_bytes).unwrap();
    assert_eq!(token.public_key(), &identity.keypair().public_key());
    assert_eq!(token.capabilities().to_string(), CAPABILITY);
}

#[tokio::test]
async fn test_approve_auth_with_companion_claim_reports_relay_failure() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = Url::parse(&format!("http://{}/inbox", listener.local_addr().unwrap())).unwrap();
    drop(listener);
    let bootstrap = PubkySessionBootstrap::new().unwrap();
    let auth_url = auth_url(&relay, &[9; 32], CLAIM_TYPE);

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
        PubkyAuthCompanionClaimApprovalError::RelayDeliveryFailure { .. }
    ));
}
