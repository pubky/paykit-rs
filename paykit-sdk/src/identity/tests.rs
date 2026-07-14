use super::*;

#[test]
fn test_pubky_local_secret_key_debug_is_redacted() {
    let key = PubkyLocalSecretKey::new([7; 32]);

    assert_eq!(format!("{key:?}"), "PubkyLocalSecretKey(<redacted>)");
}

#[test]
fn test_pubky_local_secret_key_from_hex_validates_length() {
    let key = PubkyLocalSecretKey::from_hex(&"07".repeat(32)).unwrap();

    assert_eq!(key.as_bytes(), &[7; 32]);
    assert!(PubkyLocalSecretKey::from_hex("07").is_err());
    assert!(PubkyLocalSecretKey::from_hex("not-hex").is_err());
}

#[test]
fn test_pubky_local_secret_key_derivation_matches_pubky_core_seed() {
    let seed = [3; 64];
    let key = PubkyLocalSecretKey::from_bip39_seed(&seed).unwrap();

    assert_eq!(key.as_bytes(), &[3; 32]);
    assert!(PubkyLocalSecretKey::from_bip39_seed(&seed[..32]).is_err());
}

#[test]
fn test_pubky_local_secret_key_derivation_matches_pubky_core_mnemonic() {
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let key = PubkyLocalSecretKey::from_bip39_mnemonic(mnemonic).unwrap();

    assert_eq!(
        hex::encode(key.as_bytes()),
        "5eb00bbddcf069084889a8ab9155568165f5c453ccb85e70811aaed6f6da5fc1"
    );
    assert_eq!(
        key,
        PubkyLocalSecretKey::from_bip39_seed(
            &hex::decode(
                "5eb00bbddcf069084889a8ab9155568165f5c453ccb85e70811aaed6f6da5fc1\
             9a5ac40b389cd370d086206dec8aa6c43daea6690f20ad3d8d48b2d2ce9e38e4"
            )
            .unwrap()
        )
        .unwrap()
    );
    assert!(PubkyLocalSecretKey::from_bip39_mnemonic("not a mnemonic").is_err());
}

#[test]
fn test_pubky_local_secret_key_returns_public_key() {
    let key = PubkyLocalSecretKey::new([9; 32]);
    let public_key = key.public_key();

    assert_eq!(
        public_key.to_public_key().unwrap(),
        key.keypair().public_key()
    );
}

#[test]
fn test_session_capabilities_cover_required_paykit_scopes() {
    let root = pubky::Capabilities::builder()
        .read_write("/")
        .finish()
        .as_slice()
        .to_vec();
    let paykit_only = pubky::Capabilities::builder()
        .read_write("/pub/paykit/")
        .finish()
        .as_slice()
        .to_vec();
    let bitkit_namespace = pubky::Capabilities::builder()
        .read_write("/pub/paykit/v0/paykit/wallet/")
        .read_write("/pub/paykit/v0/private/paykit/wallet/")
        .read_write("/pub/bitkit.to/")
        .finish()
        .as_slice()
        .to_vec();
    let bitkit_required = "/pub/paykit/v0/paykit/wallet/:rw,/pub/paykit/v0/private/paykit/wallet/:rw,/pub/bitkit.to/paykit/wallet/:rw";
    let read_only = pubky::Capabilities::builder()
        .read("/pub/paykit/")
        .finish()
        .as_slice()
        .to_vec();

    assert!(validate_session_capabilities(&root, "/pub/paykit/:rw").is_ok());
    assert!(validate_session_capabilities(
        &paykit_only,
        "/pub/paykit/v0/paykit/wallet/:rw,/pub/paykit/v0/private/paykit/wallet/:rw"
    )
    .is_ok());
    assert!(validate_session_capabilities(&root, bitkit_required).is_ok());
    assert!(validate_session_capabilities(&bitkit_namespace, bitkit_required).is_ok());
    assert!(validate_session_capabilities(&paykit_only, bitkit_required).is_err());
    assert!(validate_session_capabilities(&read_only, "/pub/paykit/:rw").is_err());
}

#[test]
fn test_pubky_public_key_validates_and_round_trips_z32() {
    let public_key = pubky::Keypair::random().public_key();
    let wrapped = PubkyPublicKey::new(public_key.z32()).unwrap();

    assert_eq!(wrapped.to_public_key().unwrap(), public_key);
    assert_eq!(wrapped.as_str(), public_key.z32());
}

#[test]
fn test_pubky_public_key_accepts_raw_or_app_key() {
    let public_key = pubky::Keypair::random().public_key();
    let raw = public_key.z32();
    let app_key = format!("pubky{raw}");

    let from_raw = PubkyPublicKey::from_raw_or_app_key(&raw).unwrap();
    let from_app_key = PubkyPublicKey::from_raw_or_app_key(&app_key).unwrap();

    assert_eq!(from_raw, from_app_key);
    assert_eq!(from_app_key.as_str(), raw);
    assert_eq!(from_app_key.to_app_key(), app_key);
    assert!(from_app_key.redacted_app_key().starts_with("pubky"));
}

#[test]
fn test_pubky_public_key_rejects_invalid_text() {
    let result = PubkyPublicKey::new("pk-peer");

    assert!(result.is_err());
}

#[test]
fn test_pubky_public_key_deserialization_validates() {
    let result = serde_json::from_str::<PubkyPublicKey>(r#""pk-peer""#);

    assert!(result.is_err());
}
