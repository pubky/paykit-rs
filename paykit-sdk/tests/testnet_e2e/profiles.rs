use std::time::Duration;

use paykit_sdk::{
    ContactUpdate, PaykitProfile, PaykitSdk, PaykitSdkConfig, PublicContactSharingPolicy,
    PublicationStatus, StorageAdapter,
};

use crate::harness::{two_party, TestnetSessionProvider};

#[tokio::test]
async fn test_public_directory_pages_cover_all_entries_and_missing_directories() {
    let pair = two_party().await;
    let path = "/pub/paykit/page-test-\u{e9}/";
    let resource = pubky::PubkyResource::new(pair.alice.access.session.public_key(), path).unwrap();
    let root = resource.to_pubky_url();
    let mut names: Vec<_> = (0..99).map(|index| format!("{index:03}.json")).collect();
    names.extend(
        [
            "a",
            "z",
            "\u{e9}",
            "\u{ea}",
            "100% ready",
            "%C3%A9",
            "%5C",
            "%0A",
            "a#b",
            "a?b",
        ]
        .map(String::from),
    );
    names.sort();
    let session = &pair.alice.access.session;
    let bearer = session.as_grant().unwrap().current_bearer().await;
    let mut expected = Vec::new();
    let mut contents = Vec::new();
    for name in names {
        let body = format!("contents for {name}");
        let entry =
            pubky::PubkyResource::new(resource.owner.clone(), format!("{path}{name}")).unwrap();
        // SessionStorage re-encodes ResourcePath; write the intended Unicode fixture directly.
        session
            .client()
            .request_async(reqwest::Method::PUT, entry.to_transport_url().unwrap())
            .await
            .unwrap()
            .bearer_auth(&bearer)
            .body(body.clone())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        expected.push(entry.to_pubky_url());
        contents.push((
            body,
            entry.path.as_str().to_string(),
            entry.to_transport_url().unwrap(),
        ));
    }
    let mut found = Vec::new();
    loop {
        let page = pair
            .bob
            .sdk
            .list_pubky_directory_page(&root, found.last().map(String::as_str), 100)
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 100);
        found.extend(page);
        assert!(found.len() <= expected.len());
    }
    assert_eq!(found, expected);
    for (uri, (body, path, transport_url)) in found.iter().zip(contents) {
        let parsed = paykit_sdk::parse_pubky_resource(uri).unwrap();
        let resolved = paykit_sdk::resolve_pubky_url(uri).unwrap();
        assert_eq!(parsed.public_key, pair.alice.public_key);
        assert_eq!(parsed.path, path);
        assert_eq!(parsed.transport_url, transport_url.as_str());
        assert_eq!(resolved, transport_url.as_str());
        let response = pair
            .bob
            .access
            .session
            .client()
            .request_async(reqwest::Method::GET, resolved.parse().unwrap())
            .await
            .unwrap()
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(response.bytes().await.unwrap().as_ref(), body.as_bytes());
        assert_eq!(
            pair.bob
                .sdk
                .fetch_pubky_file(uri, body.len())
                .await
                .unwrap(),
            Some(body.as_bytes().to_vec()),
            "fetch bytes from listed URI {uri}"
        );
        assert_eq!(
            pair.bob
                .sdk
                .fetch_pubky_file_bounded(uri, body.len() as u64)
                .await
                .unwrap(),
            Some(body.as_bytes().to_vec()),
            "fetch bounded bytes from listed URI {uri}"
        );
        assert_eq!(
            pair.bob
                .sdk
                .fetch_pubky_text(uri, body.len())
                .await
                .unwrap(),
            Some(body),
            "fetch text from listed URI {uri}"
        );
    }
    assert!(pair
        .bob
        .sdk
        .list_pubky_directory_page(&format!("{root}missing/"), None, 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_public_contact_markers_serialize_intent_and_recheck_pending_work() {
    let pair = two_party().await;
    let mut config = PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap();
    config.public_contact_sharing = PublicContactSharingPolicy::Enabled;
    let sdk = PaykitSdk::new(
        pair.alice.storage.clone(),
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        config,
    );
    let contact = pair.bob.public_key.clone();
    sdk.save_contact(ContactUpdate {
        public_key: contact.clone(),
        label: Some("Bob".into()),
    })
    .await
    .unwrap();
    let remote = pair.alice.access.session.storage();
    let path = format!("/pub/paykit/contacts/{contact}.json");
    let lock = remote.lock(&path, Duration::from_secs(60)).await.unwrap();

    for publish in [true, false] {
        let result = if publish {
            sdk.publish_public_contact(contact.clone()).await.map(Some)
        } else {
            sdk.remove_public_contact(contact.clone()).await
        };
        assert!(result.unwrap_err().is_concurrent_update());
        assert_eq!(
            sdk.contact_record(&contact)
                .await
                .unwrap()
                .unwrap()
                .public_contact_marker_status,
            PublicationStatus::NotPublished,
            "a competing operation must not replace marker intent before owning the lock"
        );
    }

    pair.alice
        .storage
        .transaction(|tx| {
            let mut record = tx.contact_record(&contact).unwrap();
            record.public_contact_marker_status = PublicationStatus::PendingPublication;
            tx.save_contact_record(record);
            Ok(())
        })
        .await
        .unwrap();
    let sync = sdk.sync_public_contact_markers();
    tokio::pin!(sync);
    tokio::select! {
        result = &mut sync => panic!("sync must wait for the marker lock: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    // Another holder finishes removal after sync has loaded its pending list.
    pair.alice
        .storage
        .transaction(|tx| {
            let mut record = tx.contact_record(&contact).unwrap();
            record.public_contact_marker_status = PublicationStatus::Removed;
            tx.save_contact_record(record);
            Ok(())
        })
        .await
        .unwrap();
    remote.unlock(&lock).await.unwrap();
    assert!(sync.await.unwrap().is_empty());

    assert_eq!(
        sdk.publish_public_contact(contact.clone())
            .await
            .unwrap()
            .public_contact_marker_status,
        PublicationStatus::Published
    );
    remote.get(&path).await.unwrap();
    assert_eq!(
        sdk.remove_public_contact(contact.clone())
            .await
            .unwrap()
            .unwrap()
            .public_contact_marker_status,
        PublicationStatus::Removed
    );
    let error = remote.get(&path).await.unwrap_err();
    assert!(matches!(error,
        pubky::Error::Request(pubky::errors::RequestError::Server { status, .. })
            if status == pubky::StatusCode::NOT_FOUND || status == pubky::StatusCode::GONE
    ));
    sdk.remove_contact(&contact).await.unwrap();
}

#[tokio::test]
async fn test_paykit_profile_publish_and_fetch_roundtrip() {
    let pair = two_party().await;

    let profile = PaykitProfile {
        display_name: Some("Alice".into()),
        image_uri: None,
        extra: None,
    };
    let record = pair
        .alice
        .sdk
        .publish_paykit_profile(profile.clone(), None)
        .await
        .expect("publishing the profile should succeed");
    assert_eq!(record.public_key, pair.alice.public_key);

    let fetched = pair
        .bob
        .sdk
        .fetch_paykit_profile(pair.alice.public_key.clone())
        .await
        .expect("fetching the profile should succeed")
        .expect("the published profile should be present");
    assert_eq!(fetched.profile, profile);
    assert_eq!(fetched.public_key, pair.alice.public_key);
    assert_eq!(fetched.revision, record.revision);

    let updated_profile = PaykitProfile {
        display_name: Some("Alice Updated".into()),
        image_uri: None,
        extra: None,
    };
    let updated = pair
        .alice
        .sdk
        .publish_paykit_profile(updated_profile, Some(record.revision.clone()))
        .await
        .expect("the current profile revision should update");
    assert_ne!(updated.revision, record.revision);
    let stale = pair
        .alice
        .sdk
        .publish_paykit_profile(profile.clone(), Some(record.revision))
        .await
        .expect_err("a stale profile revision should not overwrite the update");
    assert!(stale.is_concurrent_update());

    // A missing profile is a real homeserver 404 mapped to Ok(None).
    let missing = pair
        .bob
        .sdk
        .fetch_paykit_profile(pair.bob.public_key.clone())
        .await
        .expect("fetching an absent profile should not error");
    assert!(missing.is_none());
}

#[tokio::test]
async fn test_paykit_blob_delete_rejects_ambiguous_uris_without_removing_blob() {
    let pair = two_party().await;
    let blob = pair
        .alice
        .sdk
        .publish_paykit_blob("ab".into(), b"preserved".to_vec())
        .await
        .unwrap();
    let prefix = blob.uri.strip_suffix("ab").unwrap();
    for separator in ["\t", "\n", "\r", "%09", "%0A", "%0D"] {
        let uri = format!("{prefix}a{separator}b");
        let result = pair.alice.sdk.delete_paykit_blob(&uri).await;
        assert_eq!(
            pair.bob.sdk.fetch_pubky_file(&blob.uri, 9).await.unwrap(),
            Some(b"preserved".to_vec()),
            "malformed URI must not delete the stored blob: {uri:?}"
        );
        assert!(matches!(
            result,
            Err(paykit_sdk::PaykitSdkError::Protocol { .. })
        ));
    }
    assert!(matches!(
        pair.bob.sdk.delete_paykit_blob(&blob.uri).await,
        Err(paykit_sdk::PaykitSdkError::Protocol { .. })
    ));
    assert_eq!(
        pair.bob.sdk.fetch_pubky_file(&blob.uri, 9).await.unwrap(),
        Some(b"preserved".to_vec())
    );
    pair.alice
        .sdk
        .delete_paykit_blob(&format!("{prefix}%61%62"))
        .await
        .unwrap();
    assert_eq!(
        pair.bob.sdk.fetch_pubky_file(&blob.uri, 9).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn test_public_file_fetch_validates_uris_and_body_limits() {
    let pair = two_party().await;
    for path in ["%FF", "x/..%5C..%5Cpriv%5Csecret", "a%0Ab"] {
        let invalid_uri = format!("pubky://{}/pub/{path}", pair.alice.public_key);
        for result in [
            pair.bob.sdk.fetch_pubky_file(&invalid_uri, 10).await,
            pair.bob
                .sdk
                .fetch_pubky_file_bounded(&invalid_uri, 10)
                .await,
            pair.bob
                .sdk
                .fetch_pubky_text(&invalid_uri, 10)
                .await
                .map(|text| text.map(String::into_bytes)),
        ] {
            assert!(matches!(
                result.unwrap_err(),
                paykit_sdk::PaykitSdkError::Protocol { .. }
            ));
        }
    }
    let blob = pair
        .alice
        .sdk
        .publish_paykit_blob("icon.png".into(), b"1234".to_vec())
        .await
        .unwrap();
    assert_eq!(
        pair.bob
            .sdk
            .fetch_pubky_file_bounded(&blob.uri, 4)
            .await
            .unwrap(),
        Some(b"1234".to_vec())
    );
    for limit in [0, 3] {
        let err = pair
            .bob
            .sdk
            .fetch_pubky_file_bounded(&blob.uri, limit)
            .await
            .unwrap_err();
        assert!(matches!(err, paykit_sdk::PaykitSdkError::Protocol { .. }));
        assert!(err.to_string().contains("fetch Pubky file"));
    }
    let empty = pair
        .alice
        .sdk
        .publish_paykit_blob("empty".into(), Vec::new())
        .await
        .unwrap();
    assert_eq!(
        pair.bob
            .sdk
            .fetch_pubky_file_bounded(&empty.uri, 0)
            .await
            .unwrap(),
        Some(Vec::new())
    );
    pair.alice.sdk.delete_paykit_blob(&empty.uri).await.unwrap();
    assert_eq!(
        pair.bob
            .sdk
            .fetch_pubky_file_bounded(&empty.uri, 0)
            .await
            .unwrap(),
        None
    );

    let large = pair
        .alice
        .sdk
        .publish_paykit_blob("large".into(), vec![0; 5 * 1024 * 1024 + 1])
        .await
        .unwrap();
    for result in [
        pair.bob
            .sdk
            .fetch_pubky_file(&large.uri, 5 * 1024 * 1024)
            .await,
        pair.bob
            .sdk
            .fetch_pubky_file_bounded(&large.uri, u64::MAX)
            .await,
    ] {
        let err = result.unwrap_err();
        assert!(matches!(err, paykit_sdk::PaykitSdkError::Protocol { .. }));
        assert!(err.to_string().contains("5242880"));
    }
}

#[tokio::test]
async fn test_pubky_profile_publish_merges_and_rejects_stale_revisions() {
    let testnet = crate::harness::build_testnet().await;
    let capabilities = format!(
        "{},{}",
        paykit_sdk::PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        paykit_sdk::PUBKY_APP_PROFILE_SESSION_CAPABILITIES
    );
    let user = crate::harness::TestUser::sign_up_with_capabilities(
        &testnet,
        crate::harness::app_id("bitkit"),
        &capabilities,
    )
    .await;
    let sdk = &user.sdk;
    let update = |name: &str| paykit_sdk::PubkyProfileUpdate {
        name: Some(name.into()),
        ..Default::default()
    };

    assert!(sdk
        .fetch_pubky_profile_with_revision(user.public_key.clone())
        .await
        .unwrap()
        .is_none());
    let created = sdk
        .publish_pubky_profile(update("Alice"), None)
        .await
        .unwrap();

    // A new identity's first profile is social v1; pubky.app adds a status and a key Paykit does not know.
    user.access
        .session
        .storage()
        .put(
            paykit_sdk::SOCIAL_V1_PROFILE_PATH,
            r#"{"name":"Alice","status":"away","theme":"dark"}"#,
        )
        .await
        .unwrap();
    let stale = sdk
        .publish_pubky_profile(update("Alicia"), Some(created.revision))
        .await
        .unwrap_err();
    assert!(stale.is_concurrent_update(), "{stale:?}");

    let current = sdk
        .fetch_pubky_profile_with_revision(user.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    let current_revision_before_publish = current.revision.clone();
    let published = sdk
        .publish_pubky_profile(update("Alicia"), Some(current.revision))
        .await
        .unwrap();
    assert_eq!(published.profile.name, "Alicia");
    assert_eq!(published.profile.status.as_deref(), Some("away"));
    let raw = user
        .access
        .session
        .storage()
        .get(paykit_sdk::SOCIAL_V1_PROFILE_PATH)
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let raw: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(raw["theme"], "dark");

    let stale = sdk
        .delete_pubky_profile(current_revision_before_publish)
        .await
        .unwrap_err();
    assert!(stale.is_concurrent_update(), "{stale:?}");
    sdk.delete_pubky_profile(published.revision).await.unwrap();
    assert!(sdk
        .fetch_pubky_profile_with_revision(user.public_key.clone())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_pubky_app_avatar_upload_writes_blob_and_file() {
    let testnet = crate::harness::build_testnet().await;
    let capabilities = format!(
        "{},{}",
        paykit_sdk::PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        paykit_sdk::PUBKY_APP_PROFILE_SESSION_CAPABILITIES
    );
    let user = crate::harness::TestUser::sign_up_with_capabilities(
        &testnet,
        crate::harness::app_id("bitkit"),
        &capabilities,
    )
    .await;
    // A pubky.app (v0) identity: its avatar is a pubky.app blob and file.
    user.access
        .session
        .storage()
        .put(paykit_sdk::PUBKY_PROFILE_PATH, r#"{"name":"Alice"}"#)
        .await
        .unwrap();
    let bytes = vec![0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];
    let file_uri = user
        .sdk
        .upload_pubky_app_avatar(bytes.clone(), "image/jpeg")
        .await
        .unwrap();
    assert!(file_uri.contains("/pub/pubky.app/files/"), "{file_uri}");

    let file = user
        .sdk
        .fetch_pubky_text(&file_uri, 4096)
        .await
        .unwrap()
        .unwrap();
    let file: serde_json::Value = serde_json::from_str(&file).unwrap();
    assert_eq!(file["content_type"], "image/jpeg");
    assert_eq!(file["size"], bytes.len());
    let blob = user
        .sdk
        .fetch_pubky_file(file["src"].as_str().unwrap(), 4096)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blob, bytes);

    let without_scope = crate::harness::two_party().await;
    let error = without_scope
        .alice
        .sdk
        .upload_pubky_app_avatar(bytes, "image/jpeg")
        .await
        .unwrap_err();
    assert!(
        matches!(error, paykit_sdk::PaykitSdkError::Identity { .. }),
        "{error:?}"
    );
    let error = without_scope
        .alice
        .sdk
        .publish_pubky_profile(
            paykit_sdk::PubkyProfileUpdate {
                name: Some("Alice".into()),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, paykit_sdk::PaykitSdkError::Identity { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_pubky_profile_follows_and_avatar_use_social_v1_once_migrated() {
    let testnet = crate::harness::build_testnet().await;
    // The broader scopes let the test write what pubky.app's migration writes.
    let capabilities = format!(
        "{},{},/priv/social/v1/:rw,/pub/social/v1/:rw,/pub/pubky.app/follows/:rw",
        paykit_sdk::PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        paykit_sdk::PUBKY_APP_PROFILE_SESSION_CAPABILITIES
    );
    let alice = crate::harness::TestUser::sign_up_with_capabilities(
        &testnet,
        crate::harness::app_id("bitkit"),
        &capabilities,
    )
    .await;
    // The runtime under test holds exactly the scopes Bitkit requests.
    let bitkit = alice
        .additional_app_with_capabilities(
            &testnet,
            crate::harness::app_id("bitkit-social"),
            "Bitkit",
            &format!(
                "{},{}",
                paykit_sdk::PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
                paykit_sdk::PUBKY_APP_PROFILE_SESSION_CAPABILITIES
            ),
        )
        .await;
    let bob = crate::harness::TestUser::sign_up(&testnet).await;
    let storage = alice.access.session.storage();
    let put = |path: String, body: String| {
        let storage = storage.clone();
        async move { storage.put(path.as_str(), body).await.unwrap() }
    };
    put(
        paykit_sdk::PUBKY_PROFILE_PATH.into(),
        r#"{"name":"Old Alice"}"#.into(),
    )
    .await;
    put(
        format!(
            "{}{}",
            paykit_sdk::PUBKY_FOLLOWS_PATH_PREFIX,
            bob.public_key
        ),
        r#"{"created_at":1}"#.into(),
    )
    .await;
    // Before the flag, the identity is on pubky.app (v0).
    let snapshot = bitkit
        .sdk
        .fetch_pubky_profile_with_revision(alice.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.profile.name, "Old Alice");

    // pubky.app's migration copies the data to social v1 and writes its flag last.
    put(
        paykit_sdk::SOCIAL_V1_PROFILE_PATH.into(),
        r#"{"name":"Alice","status":"away"}"#.into(),
    )
    .await;
    // From the social v1 profile on, Nexus reads the user as v1, so the flag is not waited for.
    let snapshot = bitkit
        .sdk
        .fetch_pubky_profile_with_revision(alice.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.profile.name, "Alice");
    // The flag lists every migrated path, so an account with many follows writes well over 4 KiB.
    let migrated: Vec<String> = (0..200)
        .map(|i| format!("/pub/social/v1/follows/{i:052}.json"))
        .collect();
    let flag = serde_json::json!({
        "migrated_at": 1,
        "transform_rev": 3,
        "skipped": [],
        "migrated": migrated,
    })
    .to_string();
    assert!(flag.len() > 8 * 1024);
    put(paykit_sdk::SOCIAL_V1_MIGRATION_FLAG_PATH.into(), flag).await;

    let snapshot = bitkit
        .sdk
        .fetch_pubky_profile_with_revision(alice.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.profile.name, "Alice");
    let published = bitkit
        .sdk
        .publish_pubky_profile(
            paykit_sdk::PubkyProfileUpdate {
                name: Some("Alicia".into()),
                ..Default::default()
            },
            Some(snapshot.revision),
        )
        .await
        .unwrap();
    assert_eq!(published.profile.status.as_deref(), Some("away"));
    let read = |path: &'static str| {
        let storage = storage.clone();
        async move {
            match storage.get(path).await {
                Ok(response) => Some(response.text().await.unwrap()),
                Err(_) => None,
            }
        }
    };
    assert!(read(paykit_sdk::SOCIAL_V1_PROFILE_PATH)
        .await
        .unwrap()
        .contains("Alicia"));
    assert!(read(paykit_sdk::PUBKY_PROFILE_PATH)
        .await
        .unwrap()
        .contains("Old Alice"));

    // Others read the social v1 profile and follows.
    let record = bob
        .sdk
        .fetch_pubky_profile(alice.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.path, paykit_sdk::SOCIAL_V1_PROFILE_PATH);
    assert!(bob
        .sdk
        .fetch_pubky_follows(alice.public_key.clone(), 100)
        .await
        .unwrap()
        .is_empty());
    put(
        format!(
            "{}{}.json",
            paykit_sdk::SOCIAL_V1_FOLLOWS_PATH_PREFIX,
            bob.public_key
        ),
        r#"{"created_at":2}"#.into(),
    )
    .await;
    for sdk in [&bitkit.sdk, &bob.sdk] {
        assert_eq!(
            sdk.fetch_pubky_follows(alice.public_key.clone(), 100)
                .await
                .unwrap(),
            vec![bob.public_key.clone()]
        );
    }

    let bytes = vec![0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];
    let uri = bitkit
        .sdk
        .upload_pubky_app_avatar(bytes.clone(), "image/jpeg")
        .await
        .unwrap();
    assert!(
        uri.contains(paykit_sdk::SOCIAL_V1_FILE_PATH_PREFIX),
        "{uri}"
    );
    assert!(uri.ends_with(".jpg"), "{uri}");
    assert_eq!(
        bob.sdk.fetch_pubky_file(&uri, 4096).await.unwrap().unwrap(),
        bytes
    );

    // A delete removes the social v1 profile and its pubky.app (v0) copy, as pubky.app does.
    bitkit
        .sdk
        .delete_pubky_profile(published.revision)
        .await
        .unwrap();
    assert!(read(paykit_sdk::SOCIAL_V1_PROFILE_PATH).await.is_none());
    assert!(read(paykit_sdk::PUBKY_PROFILE_PATH).await.is_none());
}

#[tokio::test]
async fn test_new_identity_creates_its_profile_on_social_v1_without_the_flag() {
    let testnet = crate::harness::build_testnet().await;
    let capabilities = format!(
        "{},{},/pub/pubky.app/follows/:rw",
        paykit_sdk::PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        paykit_sdk::PUBKY_APP_PROFILE_SESSION_CAPABILITIES
    );
    let sign_up = || {
        crate::harness::TestUser::sign_up_with_capabilities(
            &testnet,
            crate::harness::app_id("bitkit"),
            &capabilities,
        )
    };
    let update = paykit_sdk::PubkyProfileUpdate {
        name: Some("Alice".into()),
        ..Default::default()
    };

    let alice = sign_up().await;
    let storage = alice.access.session.storage();
    // The profile comes first: Nexus ignores a social v1 file written before it.
    let error = alice
        .sdk
        .upload_pubky_app_avatar(vec![0xff, 0xd8, 0xff, 0xe0, 1], "image/jpeg")
        .await
        .unwrap_err();
    assert!(
        matches!(error, paykit_sdk::PaykitSdkError::Protocol { .. }),
        "{error:?}"
    );
    let created = alice
        .sdk
        .publish_pubky_profile(update.clone(), None)
        .await
        .unwrap();
    assert!(storage
        .exists(paykit_sdk::SOCIAL_V1_PROFILE_PATH)
        .await
        .unwrap());
    assert!(!storage
        .exists(paykit_sdk::PUBKY_PROFILE_PATH)
        .await
        .unwrap());
    assert!(!storage
        .exists(paykit_sdk::SOCIAL_V1_MIGRATION_FLAG_PATH)
        .await
        .unwrap());
    // The avatar comes after the profile, so it is a social v1 file too.
    let uri = alice
        .sdk
        .upload_pubky_app_avatar(vec![0xff, 0xd8, 0xff, 0xe0, 1], "image/jpeg")
        .await
        .unwrap();
    assert!(
        uri.contains(paykit_sdk::SOCIAL_V1_FILE_PATH_PREFIX),
        "{uri}"
    );
    let read = alice
        .sdk
        .fetch_pubky_profile_with_revision(alice.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.revision, created.revision);

    // Any pubky.app (v0) data, a follow alone included, keeps the first profile on v0.
    let bob = sign_up().await;
    let storage = bob.access.session.storage();
    storage
        .put(
            format!(
                "{}{}",
                paykit_sdk::PUBKY_FOLLOWS_PATH_PREFIX,
                alice.public_key
            )
            .as_str(),
            r#"{"created_at":1}"#,
        )
        .await
        .unwrap();
    bob.sdk
        .publish_pubky_profile(update.clone(), None)
        .await
        .unwrap();
    assert!(storage
        .exists(paykit_sdk::PUBKY_PROFILE_PATH)
        .await
        .unwrap());
    assert!(!storage
        .exists(paykit_sdk::SOCIAL_V1_PROFILE_PATH)
        .await
        .unwrap());

    // So does a pubky.app (v0) profile: an edit of it stays on v0.
    let carol = sign_up().await;
    let storage = carol.access.session.storage();
    storage
        .put(paykit_sdk::PUBKY_PROFILE_PATH, r#"{"name":"Carol"}"#)
        .await
        .unwrap();
    let current = carol
        .sdk
        .fetch_pubky_profile_with_revision(carol.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    carol
        .sdk
        .publish_pubky_profile(update, Some(current.revision))
        .await
        .unwrap();
    assert!(!storage
        .exists(paykit_sdk::SOCIAL_V1_PROFILE_PATH)
        .await
        .unwrap());
}

#[tokio::test]
async fn test_pubky_profile_delete_removes_its_own_avatar() {
    let testnet = crate::harness::build_testnet().await;
    let capabilities = format!(
        "{},{}",
        paykit_sdk::PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        paykit_sdk::PUBKY_APP_PROFILE_SESSION_CAPABILITIES
    );
    let sign_up = || {
        crate::harness::TestUser::sign_up_with_capabilities(
            &testnet,
            crate::harness::app_id("bitkit"),
            &capabilities,
        )
    };
    let named = |name: &str| paykit_sdk::PubkyProfileUpdate {
        name: Some(name.into()),
        ..Default::default()
    };
    let with_image = |image: &str| paykit_sdk::PubkyProfileUpdate {
        image: Some(image.into()),
        ..Default::default()
    };
    let path_of = |uri: &str| paykit_sdk::parse_pubky_resource(uri).unwrap().path;
    let bytes = vec![0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];

    // Social v1: the avatar is one file.
    let alice = sign_up().await;
    let storage = alice.access.session.storage();
    let created = alice
        .sdk
        .publish_pubky_profile(named("Alice"), None)
        .await
        .unwrap();
    let uri = alice
        .sdk
        .upload_pubky_app_avatar(bytes.clone(), "image/jpeg")
        .await
        .unwrap();
    let saved = alice
        .sdk
        .publish_pubky_profile(with_image(&uri), Some(created.revision.clone()))
        .await
        .unwrap();
    // A stale revision deletes nothing.
    let error = alice
        .sdk
        .delete_pubky_profile(created.revision)
        .await
        .unwrap_err();
    assert!(
        matches!(error, paykit_sdk::PaykitSdkError::ConcurrentUpdate { .. }),
        "{error:?}"
    );
    assert!(storage.exists(path_of(&uri).as_str()).await.unwrap());
    alice
        .sdk
        .delete_pubky_profile(saved.revision)
        .await
        .unwrap();
    assert!(!storage
        .exists(paykit_sdk::SOCIAL_V1_PROFILE_PATH)
        .await
        .unwrap());
    assert!(!storage.exists(path_of(&uri).as_str()).await.unwrap());

    // pubky.app (v0): the avatar is a file and the blob it points at.
    let bob = sign_up().await;
    let storage = bob.access.session.storage();
    storage
        .put(paykit_sdk::PUBKY_PROFILE_PATH, r#"{"name":"Bob"}"#)
        .await
        .unwrap();
    let uri = bob
        .sdk
        .upload_pubky_app_avatar(bytes.clone(), "image/jpeg")
        .await
        .unwrap();
    let file: serde_json::Value =
        serde_json::from_str(&bob.sdk.fetch_pubky_text(&uri, 4096).await.unwrap().unwrap())
            .unwrap();
    let blob_path = path_of(file["src"].as_str().unwrap());
    let current = bob
        .sdk
        .fetch_pubky_profile_with_revision(bob.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    let saved = bob
        .sdk
        .publish_pubky_profile(with_image(&uri), Some(current.revision))
        .await
        .unwrap();
    bob.sdk.delete_pubky_profile(saved.revision).await.unwrap();
    assert!(!storage
        .exists(paykit_sdk::PUBKY_PROFILE_PATH)
        .await
        .unwrap());
    assert!(!storage.exists(path_of(&uri).as_str()).await.unwrap());
    assert!(!storage.exists(blob_path.as_str()).await.unwrap());

    // An image outside this identity's avatar paths, a nested one included, stays.
    let carol = sign_up().await;
    let storage = carol.access.session.storage();
    let other = "/pub/social/v1/files/keep/avatar.jpg";
    storage.put(other, bytes.clone()).await.unwrap();
    let created = carol
        .sdk
        .publish_pubky_profile(named("Carol"), None)
        .await
        .unwrap();
    let saved = carol
        .sdk
        .publish_pubky_profile(
            with_image(&format!("pubky://{}{other}", carol.public_key)),
            Some(created.revision),
        )
        .await
        .unwrap();
    carol
        .sdk
        .delete_pubky_profile(saved.revision)
        .await
        .unwrap();
    assert!(storage.exists(other).await.unwrap());

    // Another identity's avatar stays.
    let dave = sign_up().await;
    let alices = alice
        .sdk
        .publish_pubky_profile(named("Alice"), None)
        .await
        .unwrap();
    let alices_avatar = alice
        .sdk
        .upload_pubky_app_avatar(bytes.clone(), "image/jpeg")
        .await
        .unwrap();
    alice
        .sdk
        .publish_pubky_profile(with_image(&alices_avatar), Some(alices.revision))
        .await
        .unwrap();
    let created = dave
        .sdk
        .publish_pubky_profile(named("Dave"), None)
        .await
        .unwrap();
    let saved = dave
        .sdk
        .publish_pubky_profile(with_image(&alices_avatar), Some(created.revision))
        .await
        .unwrap();
    dave.sdk.delete_pubky_profile(saved.revision).await.unwrap();
    assert!(alice
        .access
        .session
        .storage()
        .exists(path_of(&alices_avatar).as_str())
        .await
        .unwrap());

    // A social v1 identity's pubky.app (v0) copy takes its avatar with it.
    let erin = sign_up().await;
    let storage = erin.access.session.storage();
    storage
        .put(paykit_sdk::PUBKY_PROFILE_PATH, r#"{"name":"Erin"}"#)
        .await
        .unwrap();
    let v0_avatar = erin
        .sdk
        .upload_pubky_app_avatar(bytes.clone(), "image/jpeg")
        .await
        .unwrap();
    storage
        .put(
            paykit_sdk::PUBKY_PROFILE_PATH,
            format!(r#"{{"name":"Erin","image":"{v0_avatar}"}}"#),
        )
        .await
        .unwrap();
    let file: serde_json::Value = serde_json::from_str(
        &erin
            .sdk
            .fetch_pubky_text(&v0_avatar, 4096)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let blob_path = path_of(file["src"].as_str().unwrap());
    storage
        .put(paykit_sdk::SOCIAL_V1_PROFILE_PATH, r#"{"name":"Erin"}"#)
        .await
        .unwrap();
    let current = erin
        .sdk
        .fetch_pubky_profile_with_revision(erin.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    erin.sdk
        .delete_pubky_profile(current.revision)
        .await
        .unwrap();
    for path in [
        paykit_sdk::PUBKY_PROFILE_PATH.to_string(),
        paykit_sdk::SOCIAL_V1_PROFILE_PATH.to_string(),
        path_of(&v0_avatar),
        blob_path,
    ] {
        assert!(!storage.exists(path.as_str()).await.unwrap(), "{path}");
    }
}
