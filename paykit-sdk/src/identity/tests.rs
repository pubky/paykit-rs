use super::*;

#[test]
fn test_pubky_local_secret_key_debug_is_redacted() {
    let key = PubkyLocalSecretKey::new([7; 32]);

    assert_eq!(format!("{key:?}"), "PubkyLocalSecretKey(<redacted>)");
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
