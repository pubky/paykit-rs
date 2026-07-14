use std::time::Duration;

use crypto_secretbox::{aead::Aead, KeyInit, XSalsa20Poly1305};
use pubky::{
    AuthToken, EncryptedHttpRelayInboxChannel, HttpRelayInboxChannel, Pubky, PubkyHttpClient,
};

use super::*;

fn test_xpub(serialized: &[u8; SERIALIZED_ACCOUNT_XPUB_LEN]) -> String {
    bs58::encode(serialized).with_check().into_string()
}

fn test_claim() -> WatchOnlyAccountClaim {
    let serialized = std::array::from_fn(|index| index as u8);
    WatchOnlyAccountClaim::new(
        BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_VERSION,
        42,
        WatchOnlyAccountAddressType::NativeSegwit,
        test_xpub(&serialized),
    )
}

fn auth_url(relay: &Url, secret: &[u8; 32], claim_type: &str) -> String {
    format!(
        "pubkyauth://signin?caps={BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY}&relay={relay}&secret={}&{BITKIT_CLAIM_QUERY_PARAMETER}={claim_type}",
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
fn test_watch_only_claim_encoding_matches_bitkit_wire_layout() {
    let claim = test_claim();
    let identity = PubkyLocalSecretKey::new([7; 32]);
    let auth_secret_text = "request-secret";

    let payload = encode_signed_claim(&claim, auth_secret_text, &identity).unwrap();

    assert_eq!(payload.len(), 148);
    assert_eq!(payload[0], BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_VERSION);
    assert_eq!(&payload[1..5], &42_u32.to_be_bytes());
    assert_eq!(payload[5], 0);
    assert_eq!(&payload[6..84], &(0_u8..78).collect::<Vec<_>>());

    let mut signable = SIGNATURE_DOMAIN.to_vec();
    signable.extend_from_slice(&Sha256::digest(auth_secret_text.as_bytes()));
    signable.extend_from_slice(&payload[..84]);
    let expected_signature = identity.keypair().sign(&signable).to_bytes();
    assert_eq!(&payload[84..], &expected_signature);
}

#[test]
fn test_watch_only_claim_rejects_invalid_input_and_auth_claim_type() {
    let invalid_claim = WatchOnlyAccountClaim::new(
        2,
        42,
        WatchOnlyAccountAddressType::NativeSegwit,
        test_claim().account_xpub,
    );
    assert!(matches!(
        encode_unsigned_claim(&invalid_claim),
        Err(WatchOnlyAccountClaimApprovalError::InvalidClaim { .. })
    ));

    let relay = Url::parse("https://relay.example/inbox").unwrap();
    let invalid_url = auth_url(&relay, &[3; 32], "unsupported-claim");
    assert!(matches!(
        parse_companion_auth_request(&invalid_url, BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY),
        Err(WatchOnlyAccountClaimApprovalError::InvalidAuthUrl { .. })
    ));
}

#[test]
fn test_companion_query_decoding_preserves_literal_plus() {
    let url = Url::parse("pubkyauth://signin?secret=one%2Ftwo+three").unwrap();

    assert_eq!(unique_query_value(&url, "secret").unwrap(), "one/two+three");
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
    let auth_url = auth_url(&inbox, &auth_secret, BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_TYPE);

    bootstrap
        .approve_auth_with_companion_claim(
            &auth_url,
            BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY,
            &identity,
            &test_claim(),
        )
        .await
        .unwrap();

    let claim_channel =
        HttpRelayInboxChannel::new(inbox.clone(), derive_companion_channel_id(&auth_secret))
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
    assert_eq!(
        token.capabilities().to_string(),
        BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY
    );
}

#[tokio::test]
async fn test_approve_auth_with_companion_claim_reports_relay_failure() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = Url::parse(&format!("http://{}/inbox", listener.local_addr().unwrap())).unwrap();
    drop(listener);
    let bootstrap = PubkySessionBootstrap::new().unwrap();
    let auth_url = auth_url(&relay, &[9; 32], BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_TYPE);

    let error = bootstrap
        .approve_auth_with_companion_claim(
            &auth_url,
            BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY,
            &PubkyLocalSecretKey::new([7; 32]),
            &test_claim(),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        WatchOnlyAccountClaimApprovalError::RelayDeliveryFailure { .. }
    ));
}
