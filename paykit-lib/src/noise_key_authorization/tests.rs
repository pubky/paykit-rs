use super::*;

#[test]
fn test_authorization_round_trip_and_owner_binding() {
    let identity = pubky::Keypair::random();
    let noise = pubky::Keypair::random().public_key();
    let record = PaykitNoiseKeyAuthorization::sign(&identity, noise, 1).unwrap();
    let json = serde_json::to_vec(&record).unwrap();
    assert_eq!(parse(&json, &identity.public_key()).unwrap(), record);
    assert!(parse(&json, &pubky::Keypair::random().public_key()).is_err());
    assert!(
        PaykitNoiseKeyAuthorization::sign(&identity, record.noise_public_key.clone(), 0).is_err()
    );
}

#[test]
fn test_authorization_rejects_tampering_and_wrong_signer() {
    let identity = pubky::Keypair::random();
    let noise = pubky::Keypair::random();
    let record = PaykitNoiseKeyAuthorization::sign(&identity, noise.public_key(), 1).unwrap();
    for (field, value) in [
        (
            "owner",
            serde_json::json!(pubky::Keypair::random().public_key()),
        ),
        (
            "noise_public_key",
            serde_json::json!(pubky::Keypair::random().public_key()),
        ),
        ("key_generation", serde_json::json!(2)),
        ("version", serde_json::json!(2)),
        ("kind", serde_json::json!("paykit.app_registry")),
        (
            "signature",
            serde_json::json!(STANDARD.encode(
                noise
                    .sign(&signing_bytes(
                        &identity.public_key(),
                        &noise.public_key(),
                        1
                    ))
                    .to_bytes()
            )),
        ),
    ] {
        let mut wire = serde_json::to_value(&record).unwrap();
        wire[field] = value;
        assert!(
            parse(&serde_json::to_vec(&wire).unwrap(), &identity.public_key()).is_err(),
            "{field}"
        );
    }
}

#[test]
fn test_authorization_generation_checks() {
    let identity = pubky::Keypair::random();
    let first =
        PaykitNoiseKeyAuthorization::sign(&identity, pubky::Keypair::random().public_key(), 1)
            .unwrap();
    let second =
        PaykitNoiseKeyAuthorization::sign(&identity, pubky::Keypair::random().public_key(), 2)
            .unwrap();
    let third =
        PaykitNoiseKeyAuthorization::sign(&identity, pubky::Keypair::random().public_key(), 3)
            .unwrap();
    assert!(first.validate_against(&first).is_ok());
    assert!(second.validate_against(&first).is_ok());
    assert!(third.validate_against(&first).is_ok());
    assert!(first.validate_against(&second).is_err());
    let conflict =
        PaykitNoiseKeyAuthorization::sign(&identity, pubky::Keypair::random().public_key(), 1)
            .unwrap();
    assert!(conflict.validate_against(&first).is_err());
}
