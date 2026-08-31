use paykit_sdk::{
    PubkyLocalSecretKey, PubkyPublicKey, PubkySessionAccess, PAYKIT_SESSION_CAPABILITIES,
};
use pubky_testnet::pubky::Keypair;

use crate::harness::{build_testnet, session_bootstrap};

const TEST_CLIENT_ID: &str = "paykit-sdk.test";

#[tokio::test]
async fn test_external_grant_signup_retries_after_account_creation() {
    let testnet = build_testnet().await;
    let pubky = testnet.sdk().expect("testnet Pubky client");
    let identity_keypair = Keypair::random();
    let identity_secret = PubkyLocalSecretKey::new(identity_keypair.secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    pubky
        .signer(identity_keypair)
        .signup(&homeserver.to_public_key().unwrap(), None)
        .await
        .expect("account setup should succeed before the retried grant flow");
    let bootstrap = session_bootstrap(&testnet, TEST_CLIENT_ID);
    let capabilities = PAYKIT_SESSION_CAPABILITIES;

    let request = bootstrap
        .start_sign_up_auth(capabilities, &homeserver, None)
        .await
        .expect("grant signup request should start");
    let authorization_url = request.authorization_url().to_owned();
    let (completed, approved) = tokio::join!(
        request.complete(Some(identity_secret.clone()), capabilities),
        bootstrap.approve_auth(&authorization_url, capabilities, &identity_secret),
    );
    approved.expect("grant signup request should be approved");
    let completed = completed.expect("grant signup request should complete");

    assert!(completed.access.session.as_grant().is_some());
    assert_eq!(completed.public_key, identity_secret.public_key());
    assert_eq!(completed.client_id, TEST_CLIENT_ID);
}

#[tokio::test]
async fn test_pending_grant_auth_survives_secure_state_restore() {
    let testnet = build_testnet().await;
    let identity_secret = PubkyLocalSecretKey::new(Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let bootstrap = session_bootstrap(&testnet, TEST_CLIENT_ID);
    let capabilities = PAYKIT_SESSION_CAPABILITIES;

    bootstrap
        .sign_up(&identity_secret, &homeserver, None, capabilities)
        .await
        .expect("grant signup should succeed");

    let request = bootstrap
        .start_sign_in_auth(capabilities)
        .await
        .expect("grant auth request should start");
    let state = request
        .save_state()
        .expect("pending grant state should be exportable");
    let authorization_url = state.authorization_url().to_owned();
    drop(request);

    let resumed = bootstrap
        .resume_auth(&state, capabilities)
        .await
        .expect("pending grant state should restore");
    let (completed, approved) = tokio::join!(
        resumed.complete(Some(identity_secret.clone()), capabilities),
        bootstrap.approve_auth(&authorization_url, capabilities, &identity_secret),
    );
    approved.expect("grant auth request should be approved");
    let completed = completed.expect("restored grant auth request should complete");

    assert!(completed.access.session.as_grant().is_some());
    assert_eq!(completed.public_key, identity_secret.public_key());
    assert_eq!(completed.client_id, TEST_CLIENT_ID);
}

#[tokio::test]
async fn test_grant_session_exports_restores_and_rejects_non_grant_session() {
    let testnet = build_testnet().await;
    let pubky = testnet.sdk().expect("testnet Pubky client");
    let identity_keypair = Keypair::random();
    let identity_secret = PubkyLocalSecretKey::new(identity_keypair.secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let bootstrap = session_bootstrap(&testnet, TEST_CLIENT_ID);
    let capabilities = PAYKIT_SESSION_CAPABILITIES;

    let signed_up = bootstrap
        .sign_up(&identity_secret, &homeserver, None, capabilities)
        .await
        .expect("grant signup should succeed");
    assert!(signed_up.access.session.as_grant().is_some());
    assert_eq!(
        session_capabilities(&signed_up.access.session),
        capabilities
    );

    let signed_in = bootstrap
        .sign_in(&identity_secret, capabilities)
        .await
        .expect("scoped local grant signin should succeed");
    assert_eq!(
        session_capabilities(&signed_in.access.session),
        capabilities
    );

    let exported = signed_up
        .export_session_secret()
        .await
        .expect("local grant session should be exportable");
    let restored = bootstrap
        .import_session(
            exported.as_str(),
            Some(identity_secret.clone()),
            capabilities,
        )
        .await
        .expect("exported grant session should restore");
    assert!(restored.access.session.as_grant().is_some());
    assert_eq!(restored.public_key, signed_up.public_key);
    assert_eq!(restored.client_id, TEST_CLIENT_ID);

    let other_bootstrap = session_bootstrap(&testnet, "other.test");
    let wrong_client_error = other_bootstrap
        .import_session(
            exported.as_str(),
            Some(identity_secret.clone()),
            capabilities,
        )
        .await
        .expect_err("a grant issued to another client ID must be rejected");
    assert!(wrong_client_error.to_string().contains("grant client ID"));

    #[allow(
        deprecated,
        reason = "the rejection test needs a legacy non-grant session"
    )]
    let cookie_session = pubky
        .signer(identity_keypair)
        .signin_cookie()
        .await
        .expect("non-grant signin should succeed for the rejection test");
    #[allow(
        deprecated,
        reason = "the rejection test needs a legacy non-grant session"
    )]
    let cookie_secret = cookie_session
        .as_cookie()
        .and_then(|cookie| cookie.export_secret())
        .expect("non-grant session should be exportable");
    let cookie_access = PubkySessionAccess {
        session: cookie_session,
        outbox_client: pubky,
        local_secret_key: Some(identity_secret.clone()),
        paykit_identity_secret_key: None,
    };
    let validation_error = cookie_access
        .validate()
        .expect_err("runtime access must reject non-grant sessions");
    assert!(validation_error.to_string().contains("grant-backed"));

    let error = bootstrap
        .import_session(&cookie_secret, Some(identity_secret), capabilities)
        .await
        .expect_err("non-grant sessions must be rejected");

    assert!(error.to_string().contains("grant-backed"));
}

fn session_capabilities(session: &pubky::PubkySession) -> String {
    session
        .info()
        .capabilities()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}
