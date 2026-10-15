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
