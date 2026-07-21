use super::*;
use crate::{PaykitReceiverPath, PaykitSdkConfig};

fn public_key() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

fn paykit_profile_path() -> String {
    crate::PaykitSdkConfig::new(crate::PaykitReceiverPath::new("paykit/wallet").unwrap())
        .paykit_profile_path()
}

fn paykit_blob_prefix() -> String {
    crate::PaykitSdkConfig::new(crate::PaykitReceiverPath::new("paykit/wallet").unwrap())
        .paykit_profile_blob_path_prefix()
}

#[test]
fn test_paykit_profile_json_round_trips() {
    let profile = PaykitProfile {
        display_name: Some("Alice".into()),
        image_uri: Some("/pub/paykit/v0/paykit/wallet/blobs/avatar.png".into()),
        extra: Some(serde_json::Map::from_iter([(
            "bio".into(),
            serde_json::Value::String("Builder".into()),
        )])),
    };

    let json = profile_json(&profile).unwrap();
    let parsed = parse_profile_json(&json).unwrap();

    assert!(json.contains(r#""kind":"paykit.profile""#));
    assert_eq!(parsed, profile);
    assert_eq!(
        parsed
            .extra
            .as_ref()
            .and_then(|extra| extra.get("bio"))
            .and_then(|value| value.as_str()),
        Some("Builder")
    );
}

#[test]
fn test_paykit_profile_json_rejects_wrong_kind() {
    let result = parse_profile_json(
        r#"{"version":1,"kind":"paykit.other","display_name":"Alice","image_uri":null}"#,
    );

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_paykit_profile_json_rejects_empty_body() {
    let result = parse_profile_json("");

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_paykit_profile_json_ignores_unknown_fields() {
    let parsed = parse_profile_json(
        r#"{"version":1,"kind":"paykit.profile","display_name":"Alice","image_uri":null,"color":"blue"}"#,
    )
    .unwrap();

    assert_eq!(parsed.display_name.as_deref(), Some("Alice"));
}

#[test]
fn test_paykit_profile_rejects_empty_display_name() {
    let profile = PaykitProfile {
        display_name: Some(" ".into()),
        image_uri: None,
        extra: None,
    };

    assert!(matches!(
        profile.validate(),
        Err(PaykitSdkError::Protocol { .. })
    ));
}

#[test]
fn test_paykit_profile_rejects_control_characters() {
    let profile = PaykitProfile {
        display_name: Some("Alice\nAdmin".into()),
        image_uri: None,
        extra: None,
    };

    assert!(matches!(
        profile.validate(),
        Err(PaykitSdkError::Protocol { .. })
    ));
}

#[test]
fn test_paykit_profile_rejects_oversized_extra() {
    let profile = PaykitProfile {
        display_name: Some("Alice".into()),
        image_uri: None,
        extra: Some(serde_json::Map::from_iter([(
            "bio".into(),
            serde_json::Value::String("x".repeat(20 * 1024)),
        )])),
    };

    assert!(matches!(
        profile.validate(),
        Err(PaykitSdkError::Protocol { .. })
    ));
}

#[test]
fn test_pubky_profile_json_parses_bitkit_shape() {
    let parsed = parse_pubky_profile_json(
        r#"{"name":"Alice","bio":"Builder","image":"pubky://alice/avatar.jpg","links":[{"title":"site","url":"https://example.com"}],"status":"online"}"#,
    )
    .unwrap();

    assert_eq!(parsed.name, "Alice");
    assert_eq!(parsed.bio.as_deref(), Some("Builder"));
    assert_eq!(parsed.links.unwrap()[0].title, "site");
}

#[test]
fn test_pubky_profile_json_rejects_control_characters_in_name() {
    let result = parse_pubky_profile_json(r#"{"name":"Alice\nAdmin"}"#);

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_pubky_profile_json_drops_invalid_optional_fields() {
    let parsed = parse_pubky_profile_json(
        r#"{"name":"Alice","bio":"Builder\nMaker","image":"\u0001","links":[{"title":"site","url":"https://example.com"},{"title":"bad\nlink","url":"https://example.com/bad"}],"status":"online\nnow"}"#,
    )
    .unwrap();

    assert_eq!(parsed.name, "Alice");
    assert_eq!(parsed.bio, None);
    assert_eq!(parsed.image, None);
    assert_eq!(parsed.status, None);
    let links = parsed.links.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].title, "site");
}

#[test]
fn test_contact_profile_resolution_from_paykit_profile() {
    let public_key = public_key();
    let record = PaykitProfileRecord {
        public_key: public_key.clone(),
        profile: PaykitProfile {
            display_name: Some("Alice".into()),
            image_uri: Some("/pub/paykit/v0/paykit/wallet/blobs/avatar.png".into()),
            extra: None,
        },
        path: paykit_profile_path(),
        updated_at: chrono::Utc::now(),
    };

    let resolution = ContactProfileResolution::from_paykit(record);

    assert_eq!(resolution.public_key, public_key);
    assert_eq!(resolution.source, ContactProfileSource::PaykitProfile);
    assert_eq!(resolution.display_name.as_deref(), Some("Alice"));
    assert!(resolution.paykit_profile.is_some());
    assert!(resolution.pubky_profile.is_none());
}

#[test]
fn test_contact_profile_resolution_from_pubky_profile() {
    let public_key = public_key();
    let record = PubkyProfileRecord {
        public_key: public_key.clone(),
        profile: PubkyProfile {
            name: "Alice".into(),
            bio: Some("Builder".into()),
            image: Some("pubky://alice/avatar.jpg".into()),
            links: None,
            status: None,
        },
        path: PUBKY_PROFILE_PATH.into(),
        fetched_at: chrono::Utc::now(),
    };

    let resolution = ContactProfileResolution::from_pubky(record);

    assert_eq!(resolution.public_key, public_key);
    assert_eq!(resolution.source, ContactProfileSource::PubkyProfile);
    assert_eq!(resolution.display_name.as_deref(), Some("Alice"));
    assert_eq!(
        resolution.image_uri.as_deref(),
        Some("pubky://alice/avatar.jpg")
    );
    assert!(resolution.paykit_profile.is_none());
    assert!(resolution.pubky_profile.is_some());
}

#[test]
fn test_contact_profile_resolution_debug_redacts_display_data() {
    let resolution = ContactProfileResolution::from_pubky(PubkyProfileRecord {
        public_key: public_key(),
        profile: PubkyProfile {
            name: "Alice".into(),
            bio: None,
            image: Some("pubky://alice/avatar.jpg".into()),
            links: None,
            status: None,
        },
        path: PUBKY_PROFILE_PATH.into(),
        fetched_at: chrono::Utc::now(),
    });
    let debug = format!("{resolution:?}");

    assert!(!debug.contains("Alice"));
    assert!(!debug.contains("avatar.jpg"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn test_contact_update_allows_empty_label_to_clear_display_text() {
    let update = ContactUpdate {
        public_key: public_key(),
        receiver_paths: vec![receiver_path()],
        label: Some(String::new()),
    };

    assert!(update.validate().is_ok());
}

#[test]
fn test_contact_update_rejects_control_characters() {
    let update = ContactUpdate {
        public_key: public_key(),
        receiver_paths: vec![receiver_path()],
        label: Some("Alice\tLocal".into()),
    };

    assert!(matches!(
        update.validate(),
        Err(PaykitSdkError::Protocol { .. })
    ));
}

#[test]
fn test_contact_update_debug_redacts_label() {
    let public_key = public_key();
    let public_key_text = public_key.to_string();
    let update = ContactUpdate {
        public_key,
        receiver_paths: vec![receiver_path()],
        label: Some("Alice Local".into()),
    };
    let debug = format!("{update:?}");

    assert!(!debug.contains("Alice Local"));
    assert!(!debug.contains(&public_key_text));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn test_contact_record_normalizes_whitespace_labels() {
    let public_key = public_key();
    let labeled = ContactRecord::from_update(
        ContactUpdate {
            public_key: public_key.clone(),
            receiver_paths: vec![receiver_path()],
            label: Some("  Alice  ".into()),
        },
        None,
        chrono::Utc::now(),
    );
    let cleared = ContactRecord::from_update(
        ContactUpdate {
            public_key,
            receiver_paths: vec![receiver_path()],
            label: Some("   ".into()),
        },
        Some(labeled.clone()),
        chrono::Utc::now(),
    );

    assert_eq!(labeled.label.as_deref(), Some("Alice"));
    assert_eq!(cleared.label, None);
}

#[test]
fn test_pending_public_contact_marker_may_exist_remotely() {
    let record = ContactRecord::from_update(
        ContactUpdate {
            public_key: public_key(),
            receiver_paths: vec![receiver_path()],
            label: None,
        },
        None,
        chrono::Utc::now(),
    )
    .mark_public_contact_publication_pending(receiver_path(), chrono::Utc::now());

    assert!(record.may_have_public_marker());
}

#[test]
fn test_public_contact_path_uses_default_profile_namespace() {
    let public_key = public_key();
    let config = PaykitSdkConfig::new(PaykitReceiverPath::new("bitkit/wallet").unwrap());

    assert_eq!(
        config.public_contact_path(
            &public_key,
            &PaykitReceiverPath::new("tether/wallet").unwrap()
        ),
        format!(
            "/pub/paykit/v0/bitkit/wallet/contacts/{}/tether/wallet.json",
            public_key.as_str()
        )
    );
}

#[test]
fn test_public_contact_json_includes_receiver_path() {
    let json = public_contact_json(&public_key(), &receiver_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(value["receiver_path"], "bitkit/wallet");
}

#[test]
fn test_paykit_blob_path_and_uri_are_scoped_to_configured_prefix() {
    let public_key = public_key();
    let path = paykit_blob_path("/pub/bitkit.to/blobs/", "avatar-1.jpg").unwrap();

    assert_eq!(path, "/pub/bitkit.to/blobs/avatar-1.jpg");
    assert_eq!(
        paykit_blob_uri(&public_key, &path),
        format!("pubky://{}/pub/bitkit.to/blobs/avatar-1.jpg", public_key)
    );
}

#[test]
fn test_paykit_blob_name_rejects_path_segments() {
    assert!(matches!(
        paykit_blob_path(&paykit_blob_prefix(), "../avatar.jpg"),
        Err(PaykitSdkError::Protocol { .. })
    ));
    assert!(matches!(
        paykit_blob_path(&paykit_blob_prefix(), "avatars/avatar.jpg"),
        Err(PaykitSdkError::Protocol { .. })
    ));
}

#[test]
fn test_paykit_blob_path_from_uri_or_path_accepts_owned_blob_only() {
    let owner_public_key = public_key();
    let other_public_key = public_key();
    let prefix = paykit_blob_prefix();

    assert_eq!(
        paykit_blob_path_from_uri_or_path(
            &owner_public_key,
            &prefix,
            "/pub/paykit/v0/paykit/wallet/blobs/avatar.jpg"
        )
        .unwrap(),
        "/pub/paykit/v0/paykit/wallet/blobs/avatar.jpg"
    );
    assert_eq!(
        paykit_blob_path_from_uri_or_path(
            &owner_public_key,
            &prefix,
            &format!(
                "pubky://{}/pub/paykit/v0/paykit/wallet/blobs/avatar.jpg",
                owner_public_key
            )
        )
        .unwrap(),
        "/pub/paykit/v0/paykit/wallet/blobs/avatar.jpg"
    );
    assert!(matches!(
        paykit_blob_path_from_uri_or_path(
            &owner_public_key,
            &prefix,
            &format!(
                "pubky://{}/pub/paykit/v0/paykit/wallet/blobs/avatar.jpg",
                other_public_key
            )
        ),
        Err(PaykitSdkError::Protocol { .. })
    ));
    assert!(matches!(
        paykit_blob_path_from_uri_or_path(&owner_public_key, &prefix, &paykit_profile_path()),
        Err(PaykitSdkError::Protocol { .. })
    ));
}

#[test]
fn test_pubky_follow_keys_from_follow_entries_keeps_direct_valid_keys_only() {
    let owner = pubky::Keypair::random().public_key();
    let alice = pubky::Keypair::random().public_key().z32();
    let bob = pubky::Keypair::random().public_key().z32();
    let resource = |path: String| pubky::PubkyResource::new(owner.clone(), path).unwrap();

    let contacts = pubky_follow_keys_from_follow_entries(vec![
        resource(format!("{PUBKY_FOLLOWS_PATH_PREFIX}{bob}")),
        resource(format!("{PUBKY_FOLLOWS_PATH_PREFIX}{alice}/nested.json")),
        resource(format!("{PUBKY_FOLLOWS_PATH_PREFIX}not-a-public-key")),
        resource(format!("{PUBKY_FOLLOWS_PATH_PREFIX}{bob}")),
        resource("/pub/pubky.app/profile.json".to_string()),
    ]);

    assert_eq!(contacts, vec![PubkyPublicKey::new(bob).unwrap()]);
}

#[test]
fn test_pubky_paths_are_read_only_pubky_app_namespace() {
    assert_eq!(PUBKY_PROFILE_PATH, "/pub/pubky.app/profile.json");
    assert_eq!(PUBKY_FOLLOWS_PATH_PREFIX, "/pub/pubky.app/follows/");
}
