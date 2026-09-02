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
