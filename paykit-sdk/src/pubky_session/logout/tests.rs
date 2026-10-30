use pubky::{Capability, ClientId, GrantId, GrantSessionInfo};

use super::*;

mod transport;

fn original() -> (GrantSessionInfo, String) {
    original_at(Keypair::random().public_key())
}

fn original_at(homeserver: pubky::PublicKey) -> (GrantSessionInfo, String) {
    let owner = Keypair::random();
    let signer = Keypair::random();
    let now = chrono::Utc::now().timestamp() as u64;
    let info = GrantSessionInfo {
        homeserver,
        pubky: owner.public_key(),
        client_id: ClientId::new("bitkit.test").unwrap(),
        capabilities: vec![Capability::root()],
        grant_id: GrantId::generate(),
        token_expires_at: 0,
        grant_expires_at: 0,
        created_at: now,
    };
    let claims = GrantClaims {
        iss: info.pubky.clone(),
        client_id: info.client_id.clone(),
        caps: info.capabilities.clone(),
        cnf: signer.public_key(),
        jti: info.grant_id.clone(),
        iat: now - 60,
        exp: now - 1,
    };
    let secret = format!(
        "pubky-grant-credential-v1:{}:{}:{}",
        info.homeserver.z32(),
        URL_SAFE_NO_PAD.encode(signer.secret_key()),
        claims.sign(&owner, pubky::GRANT_JWS_TYP)
    );
    (info, secret)
}

#[test]
fn test_original_signer_allows_expired_original_grant_without_refresh() {
    let (info, secret) = original();
    let (grant, signer) = original_signer(&secret, &info).unwrap();
    let claims = GrantClaims::decode(&grant).unwrap();
    assert_eq!(claims.jti, info.grant_id);
    assert_eq!(claims.cnf, signer.public_key());
    assert!(pubky::GrantCredential::from_shared_secret(&secret).is_ok());
}

#[test]
fn test_original_signer_rejects_replacement_binding_or_signer() {
    let (info, secret) = original();
    for field in 0..4 {
        let mut other = info.clone();
        match field {
            0 => other.pubky = Keypair::random().public_key(),
            1 => other.client_id = ClientId::new("replacement.test").unwrap(),
            2 => other.grant_id = GrantId::generate(),
            3 => other.homeserver = Keypair::random().public_key(),
            _ => unreachable!(),
        }
        assert!(original_signer(&secret, &other).is_err());
    }
    let mut parts: Vec<_> = secret.split(':').map(str::to_owned).collect();
    parts[2] = URL_SAFE_NO_PAD.encode(Keypair::random().secret_key());
    assert!(original_signer(&parts.join(":"), &info).is_err());
}

#[test]
fn test_original_signer_rejects_unknown_or_incomplete_format() {
    let (info, secret) = original();
    for malformed in [
        String::new(),
        secret.replace("-v1:", "-v9:"),
        format!("{secret}:unexpected"),
        secret.replace("-v1:", "-v2:"),
    ] {
        assert!(original_signer(&malformed, &info).is_err());
    }
}

#[tokio::test]
async fn test_capture_is_offline_and_redacted() {
    let (info, secret) = original();
    let client = pubky::PubkyHttpClient::builder()
        .pkarr(|builder| {
            builder
                .no_default_network()
                .relays(&["http://127.0.0.1:1"])
                .unwrap()
        })
        .build()
        .unwrap();
    let session = PubkySession::from_grant_credential(
        client,
        pubky::GrantCredential::from_shared_secret(&secret).unwrap(),
    );
    let logout = PubkyGrantLogout::capture(&session, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(logout.binding().grant_id, info.grant_id.to_string());
    assert_eq!(format!("{logout:?}"), "PubkyGrantLogout(<redacted>)");
    assert_eq!(logout.acknowledged(), None);
}

#[tokio::test]
async fn test_proof_logout_revokes_rotated_original_and_preserves_different_grant() {
    use crate::{PubkyLocalSecretKey, PubkySessionBootstrap, PAYKIT_SESSION_CAPABILITIES};
    use pubky_testnet::{docker_postgres::DockerPostgres, EphemeralTestnet};

    let builder = if std::env::var_os("TEST_PUBKY_CONNECTION_STRING").is_some() {
        EphemeralTestnet::builder()
    } else {
        EphemeralTestnet::builder()
            .postgres(DockerPostgres::shared().await.connection_string().unwrap())
    };
    let testnet = builder.with_http_relay().build().await.unwrap();
    let pubky = testnet.sdk().unwrap();
    let identity = PubkyLocalSecretKey::new(Keypair::random().secret_key());
    let bootstrap = PubkySessionBootstrap::with_pubky(pubky.clone(), "bitkit.test")
        .unwrap()
        .with_auth_relay(
            testnet
                .http_relay()
                .local_url()
                .join("inbox")
                .unwrap()
                .as_str(),
        )
        .unwrap();
    let result = bootstrap
        .sign_up(
            &identity,
            &PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key()),
            None,
            PAYKIT_SESSION_CAPABILITIES,
        )
        .await
        .unwrap();
    let secret = result.export_session_secret().await.unwrap();
    let logout = PubkyGrantLogout::capture(&result.access.session, Duration::from_secs(10))
        .await
        .unwrap();
    let duplicate = PubkyGrantLogout::capture(&result.access.session, Duration::from_secs(10))
        .await
        .unwrap();
    let rotated = pubky.restore_session(secret.as_str()).await.unwrap();
    assert!(rotated.revalidate().await.unwrap().is_some());
    let other = bootstrap
        .sign_in(&identity, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap();
    assert_ne!(
        other
            .access
            .session
            .as_grant()
            .unwrap()
            .grant_id()
            .await
            .to_string(),
        logout.binding().grant_id
    );

    let attempt = logout.prepare_attempt().await.unwrap();
    assert!(logout.prepare_attempt().await.is_err());
    let receipt = attempt.execute().await.unwrap();
    assert_eq!(logout.acknowledged(), Some(receipt.clone()));
    assert!(logout.prepare_attempt().await.is_err());
    assert!(matches!(pubky.restore_session(secret.as_str()).await,
        Err(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
            if status == StatusCode::UNAUTHORIZED));
    assert!(other.access.session.revalidate().await.unwrap().is_some());
    // An independently retained original-grant intent can acknowledge the
    // already-revoked grant using a fresh PoP nonce, without any bearer restore.
    assert_eq!(
        duplicate
            .prepare_attempt()
            .await
            .unwrap()
            .execute()
            .await
            .unwrap(),
        receipt
    );
    assert!(other.access.session.revalidate().await.unwrap().is_some());
}
