use super::*;

fn public_key() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

#[test]
fn test_profile_json_round_trips() {
    let profile = PaykitProfile {
        display_name: Some("Alice".into()),
        image_uri: Some("/pub/paykit/blobs/avatar.png".into()),
    };

    let json = profile_json(&profile).unwrap();
    let parsed = parse_profile_json(&json).unwrap();

    assert!(json.contains(r#""kind":"paykit.profile""#));
    assert_eq!(parsed, profile);
}

#[test]
fn test_profile_json_rejects_wrong_kind() {
    let result = parse_profile_json(
        r#"{"version":1,"kind":"paykit.other","display_name":"Alice","image_uri":null}"#,
    );

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[test]
fn test_profile_json_ignores_unknown_fields() {
    let parsed = parse_profile_json(
        r#"{"version":1,"kind":"paykit.profile","display_name":"Alice","image_uri":null,"color":"blue"}"#,
    )
    .unwrap();

    assert_eq!(parsed.display_name.as_deref(), Some("Alice"));
}

#[test]
fn test_profile_rejects_empty_display_name() {
    let profile = PaykitProfile {
        display_name: Some(" ".into()),
        image_uri: None,
    };

    assert!(matches!(
        profile.validate(),
        Err(PaykitSdkError::Protocol(_))
    ));
}

#[test]
fn test_profile_rejects_control_characters() {
    let profile = PaykitProfile {
        display_name: Some("Alice\nAdmin".into()),
        image_uri: None,
    };

    assert!(matches!(
        profile.validate(),
        Err(PaykitSdkError::Protocol(_))
    ));
}

#[test]
fn test_contact_update_allows_empty_label_to_clear_display_text() {
    let update = ContactUpdate {
        public_key: public_key(),
        label: Some(String::new()),
    };

    assert!(update.validate().is_ok());
}

#[test]
fn test_contact_update_rejects_control_characters() {
    let update = ContactUpdate {
        public_key: public_key(),
        label: Some("Alice\tLocal".into()),
    };

    assert!(matches!(
        update.validate(),
        Err(PaykitSdkError::Protocol(_))
    ));
}

#[test]
fn test_contact_update_debug_redacts_label() {
    let public_key = public_key();
    let public_key_text = public_key.to_string();
    let update = ContactUpdate {
        public_key,
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
            label: Some("  Alice  ".into()),
        },
        None,
        chrono::Utc::now(),
    );
    let cleared = ContactRecord::from_update(
        ContactUpdate {
            public_key,
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
            label: None,
        },
        None,
        chrono::Utc::now(),
    )
    .mark_public_contact_publication_pending(chrono::Utc::now());

    assert!(record.may_have_public_marker());
}

#[test]
fn test_public_contact_path_is_under_paykit_namespace() {
    let public_key = public_key();

    assert_eq!(
        public_contact_path(&public_key),
        format!("/pub/paykit/contacts/{}.json", public_key.as_str())
    );
}
