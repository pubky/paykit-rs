use paykit_sdk::{
    PaykitReceiverPath, PaykitSdkConfig, PubkyLocalSecretKey, PubkyPublicKey,
    PubkySessionBootstrap, ReceiverNoiseSecretKey,
};
use pubky_testnet::pubky::Keypair;

use crate::harness::build_testnet;

const TEST_CLIENT_ID: &str = "paykit-sdk.test";

#[tokio::test]
async fn test_pending_grant_auth_survives_secure_state_restore() {
    let testnet = build_testnet().await;
    let pubky = testnet.sdk().expect("testnet Pubky client");
    let identity_secret = PubkyLocalSecretKey::new(Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let bootstrap = PubkySessionBootstrap::with_pubky(pubky, TEST_CLIENT_ID)
        .expect("test client ID should be valid");
    let capabilities = PaykitSdkConfig::new(
        PaykitReceiverPath::new("bitkit/wallet").expect("test receiver path should be valid"),
    )
    .required_session_capabilities();
    let receiver_noise_secret = ReceiverNoiseSecretKey::random();

    bootstrap
        .sign_up(
            &identity_secret,
            receiver_noise_secret.clone(),
            &homeserver,
            None,
            &capabilities,
        )
        .await
        .expect("grant signup should succeed");

    let request = bootstrap
        .start_sign_in_auth(&capabilities)
        .await
        .expect("grant auth request should start");
    let state = request
        .save_state()
        .expect("pending grant state should be exportable");
    let authorization_url = state.authorization_url().to_owned();
    drop(request);

    let resumed = bootstrap
        .resume_auth(&state, &capabilities)
        .await
        .expect("pending grant state should restore");
    let (completed, approved) = tokio::join!(
        resumed.complete(
            Some(identity_secret.clone()),
            receiver_noise_secret,
            &capabilities,
        ),
        bootstrap.approve_auth(&authorization_url, &capabilities, &identity_secret),
    );
    approved.expect("grant auth request should be approved");
    let completed = completed.expect("restored grant auth request should complete");

    assert!(completed.access.session.as_grant().is_some());
    assert_eq!(completed.public_key, identity_secret.public_key());
    assert_eq!(completed.client_id, TEST_CLIENT_ID);
}

#[tokio::test]
async fn test_grant_session_exports_restores_and_rejects_cookie_secret() {
    let testnet = build_testnet().await;
    let pubky = testnet.sdk().expect("testnet Pubky client");
    let identity_keypair = Keypair::random();
    let identity_secret = PubkyLocalSecretKey::new(identity_keypair.secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let bootstrap = PubkySessionBootstrap::with_pubky(pubky.clone(), TEST_CLIENT_ID)
        .expect("test client ID should be valid");
    let capabilities = PaykitSdkConfig::new(
        PaykitReceiverPath::new("bitkit/wallet").expect("test receiver path should be valid"),
    )
    .required_session_capabilities();
    let receiver_noise_secret = ReceiverNoiseSecretKey::random();

    let signed_up = bootstrap
        .sign_up(
            &identity_secret,
            receiver_noise_secret.clone(),
            &homeserver,
            None,
            &capabilities,
        )
        .await
        .expect("grant signup should succeed");
    assert!(signed_up.access.session.as_grant().is_some());

    let exported = signed_up
        .export_session_secret()
        .await
        .expect("local grant session should be exportable");
    let restored = bootstrap
        .import_session(
            exported.as_str(),
            Some(identity_secret.clone()),
            receiver_noise_secret.clone(),
            &capabilities,
        )
        .await
        .expect("exported grant session should restore");
    assert!(restored.access.session.as_grant().is_some());
    assert_eq!(restored.public_key, signed_up.public_key);
    assert_eq!(restored.client_id, TEST_CLIENT_ID);

    let other_bootstrap = PubkySessionBootstrap::with_pubky(pubky.clone(), "other.test")
        .expect("alternate test client ID should be valid");
    let wrong_client_error = other_bootstrap
        .import_session(
            exported.as_str(),
            Some(identity_secret.clone()),
            receiver_noise_secret.clone(),
            &capabilities,
        )
        .await
        .expect_err("a grant issued to another client ID must be rejected");
    assert!(wrong_client_error.to_string().contains("grant client ID"));

    let cookie_session = pubky
        .signer(identity_keypair)
        .signin_cookie()
        .await
        .expect("legacy cookie signin should succeed for the rejection test");
    let cookie_secret = cookie_session
        .as_cookie()
        .and_then(|cookie| cookie.export_secret())
        .expect("native cookie session should be exportable");
    let error = bootstrap
        .import_session(
            &cookie_secret,
            Some(identity_secret),
            receiver_noise_secret,
            &capabilities,
        )
        .await
        .expect_err("legacy cookie sessions must be rejected");

    assert!(error.to_string().contains("legacy Pubky cookie sessions"));
}
