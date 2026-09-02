use paykit_sdk::PaykitProfile;

use crate::harness::two_party;

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
        assert!(err.to_string().contains("5242880 bytes"));
    }
}
