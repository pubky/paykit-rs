use std::time::Duration;

use paykit_sdk::{
    ContactUpdate, PaykitProfile, PaykitSdk, PaykitSdkConfig, PublicContactSharingPolicy,
    PublicationStatus, StorageAdapter,
};

use crate::harness::{two_party, TestnetSessionProvider};

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
async fn test_public_file_fetch_enforces_caller_and_sdk_limits() {
    let pair = two_party().await;
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
