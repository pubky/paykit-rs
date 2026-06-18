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
fn test_pubky_local_secret_key_derivation_is_deterministic() {
    let seed = [3; 64];
    let first = PubkyLocalSecretKey::derive_from_seed(&seed, "bitkit.to").unwrap();
    let second = PubkyLocalSecretKey::derive_from_seed(&seed, "bitkit.to").unwrap();
    let other_app = PubkyLocalSecretKey::derive_from_seed(&seed, "paykit.example").unwrap();

    assert_eq!(first.as_bytes(), second.as_bytes());
    assert_eq!(
        hex::encode(first.as_bytes()),
        "7cd9a283688abc70e2cb0a13bb7aa4826ee4d7972f3070d4dade0706a83c5dee"
    );
    assert_eq!(
        first.public_key().as_str(),
        "wifjcot1pyutzt4g5jbuz9mweuh53cxyngrj4dbom6684pppzhwy"
    );
    assert_ne!(first.as_bytes(), other_app.as_bytes());
    assert_ne!(first.as_bytes(), &[3; 32]);
    assert!(PubkyLocalSecretKey::derive_from_seed(&seed[..32], "bitkit.to").is_err());
    assert!(PubkyLocalSecretKey::derive_from_seed(&seed, "").is_err());
    assert!(PubkyLocalSecretKey::derive_from_seed(&seed, "bad\nlabel").is_err());
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
        .read_write("/pub/paykit/")
        .read_write("/pub/bitkit.to/")
        .finish()
        .as_slice()
        .to_vec();
    let bitkit_required = "/pub/paykit/:rw,/pub/bitkit.to/:rw";
    let read_only = pubky::Capabilities::builder()
        .read("/pub/paykit/")
        .finish()
        .as_slice()
        .to_vec();

    assert!(validate_session_capabilities(&root, "/pub/paykit/:rw").is_ok());
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
fn test_pubky_public_key_rejects_invalid_text() {
    let result = PubkyPublicKey::new("pk-peer");

    assert!(result.is_err());
}

#[test]
fn test_pubky_public_key_deserialization_validates() {
    let result = serde_json::from_str::<PubkyPublicKey>(r#""pk-peer""#);

    assert!(result.is_err());
}
