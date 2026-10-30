use super::*;

const TEST_PUBLIC_KEY: &str = "5jsjx1o6fzu6aeeo697r3i5rx15zq41kikcye8wtwdqm4nb4tryo";
const TEST_CLIENT_ID: &str = "paykit.test";
const TEST_AUTH_SECRET: &str = "e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3s";

fn sign_in_auth_url(capabilities: &str) -> String {
    sign_in_auth_url_for_client_id(capabilities, TEST_CLIENT_ID)
}

fn sign_in_auth_url_for_client_id(capabilities: &str, client_id: &str) -> String {
    format!(
        "pubkyauth://signin_grant?caps={capabilities}&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}&cid={client_id}&cpk={TEST_PUBLIC_KEY}"
    )
}

fn sign_up_auth_url_for_client_id(capabilities: &str, client_id: &str) -> String {
    format!(
        "pubkyauth://signup_grant?caps={capabilities}&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}&hs={TEST_PUBLIC_KEY}&cid={client_id}&cpk={TEST_PUBLIC_KEY}"
    )
}

fn sign_in_auth_state_url(capabilities: &str, client_key_secret: &[u8; 32]) -> String {
    let client_public_key = pubky::Keypair::from_secret(client_key_secret).public_key();
    format!(
        "pubkyauth://signin_grant?caps={capabilities}&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}&cid={TEST_CLIENT_ID}&cpk={}",
        client_public_key.z32()
    )
}

#[test]
fn test_parse_pubky_auth_url_sign_in_grant() {
    let details = parse_pubky_auth_url(&sign_in_auth_url("/:rw")).unwrap();

    assert_eq!(details.kind, PubkyAuthRequestKind::SignIn);
    assert_eq!(details.capabilities, "/:rw");
    assert_eq!(details.relay_url, "https://httprelay.pubky.app/inbox/");
    assert_eq!(details.client_id, TEST_CLIENT_ID);
    assert!(details.homeserver_public_key.is_none());
}

#[test]
fn test_parse_pubky_auth_url_sign_up_grant() {
    let homeserver = PubkyPublicKey::new(TEST_PUBLIC_KEY).unwrap();
    let details = parse_pubky_auth_url(&format!(
        "pubkyauth://signup_grant?caps=/:rw&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}&hs={TEST_PUBLIC_KEY}&st=invite&cid={TEST_CLIENT_ID}&cpk={TEST_PUBLIC_KEY}"
    ))
    .unwrap();

    assert_eq!(details.kind, PubkyAuthRequestKind::SignUp);
    assert_eq!(details.capabilities, "/:rw");
    assert_eq!(details.client_id, TEST_CLIENT_ID);
    assert_eq!(details.homeserver_public_key.as_ref(), Some(&homeserver));
    assert!(!format!("{details:?}").contains("invite"));
    assert!(!serde_json::to_string(&details).unwrap().contains("invite"));
}

#[test]
fn test_parse_pubky_auth_url_rejects_invalid_url() {
    assert!(parse_pubky_auth_url("not-an-auth-url").is_err());
    assert!(parse_pubky_auth_url(&sign_in_auth_url("/:rw,not-a-capability")).is_err());
}

#[test]
fn test_parse_pubky_auth_url_requires_grant_auth() {
    assert!(parse_pubky_auth_url(&format!(
        "pubkyauth://signin?caps=/:rw&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}"
    ))
    .is_err());
}

#[test]
fn test_parse_pubky_auth_url_rejects_duplicate_grant_parameters() {
    let auth_url = format!("{}&cid=other.test", sign_in_auth_url("/:rw"));

    assert!(parse_pubky_auth_url(&auth_url).is_err());
}

#[test]
fn test_parse_pubky_resource_normalizes_uri() {
    let public_key = PubkyLocalSecretKey::new([4; 32]).public_key();
    for path in [
        "/pub/paykit/profile.json",
        "/pub/dir/100%25%20ready",
        "/pub/dir/%C3%A9",
        "/pub/dir/%25C3%25A9",
        "/pub/dir/%255C",
        "/pub/dir/%250A",
        "/pub/dir/a%23b",
        "/pub/dir/a%3Fb",
    ] {
        let uri = format!("pubky://{}{path}", public_key.as_str());
        let resource = parse_pubky_resource(&uri).unwrap();
        let transport_url = format!(
            "https://_pubky.{}/storage/{}{path}",
            public_key.as_str(),
            public_key.as_str()
        );
        assert_eq!(resource.public_key, public_key);
        assert_eq!(resource.path, path);
        assert_eq!(resource.transport_url, transport_url);
        assert_eq!(resolve_pubky_url(&uri).unwrap(), transport_url);
    }
    let names = (b' '..=b'~')
        .filter(|byte| !matches!(byte, b'/' | b'\\'))
        .map(|byte| format!("a{}b", char::from(byte)))
        .chain(["\u{e9}", "\u{6f22}", "%2F", "%09", "%252e%252e"].map(String::from));
    for name in names {
        let encoded =
            percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC);
        let path = format!("/pub/dir/{encoded}");
        for uri in [
            format!("pubky://{public_key}{path}"),
            format!("pubky{public_key}{path}"),
        ] {
            let parsed = parse_pubky_resource(&uri).unwrap();
            assert_eq!(parsed.public_key, public_key);
            assert_eq!(
                percent_encoding::percent_decode_str(&parsed.path)
                    .decode_utf8()
                    .unwrap(),
                format!("/pub/dir/{name}")
            );
            let transport: url::Url = parsed.transport_url.parse().unwrap();
            assert_eq!(
                transport.host_str(),
                Some(format!("_pubky.{public_key}").as_str())
            );
            assert_eq!(
                transport.path(),
                format!("/storage/{public_key}{}", parsed.path)
            );
            assert_eq!(resolve_pubky_url(&uri).unwrap(), parsed.transport_url);
        }
    }
}

#[test]
fn test_parse_pubky_resource_accepts_pubky_identifier_form() {
    let public_key = PubkyLocalSecretKey::new([5; 32]).public_key();
    let resource = parse_pubky_resource(&format!(
        "pubky{}/pub/paykit/profile.json",
        public_key.as_str()
    ))
    .unwrap();

    assert_eq!(resource.public_key, public_key);
    assert_eq!(resource.path, "/pub/paykit/profile.json");
}

#[test]
fn test_pubky_resource_helpers_reject_invalid_uris() {
    let public_key = PubkyLocalSecretKey::new([6; 32]).public_key();

    let mut uris = vec![
        public_key.as_str().to_string(),
        format!("pubky://{}/pub/%FF", public_key.as_str()),
        format!(
            "pubky://{}/pub/x/..%5C..%5Cpriv%5Csecret",
            public_key.as_str()
        ),
        format!("pubky://{}/pub/x/..\\..\\priv\\secret", public_key.as_str()),
    ];
    for byte in (0u8..=0x1f).chain([0x7f]) {
        uris.push(format!(
            "pubky://{}/pub/a{}b",
            public_key.as_str(),
            char::from(byte)
        ));
        uris.push(format!("pubky://{}/pub/a%{byte:02X}b", public_key.as_str()));
    }
    for uri in uris {
        assert!(
            matches!(
                parse_pubky_resource(&uri),
                Err(PaykitSdkError::Protocol { .. })
            ),
            "parse accepted {uri:?}"
        );
        assert!(
            matches!(
                resolve_pubky_url(&uri),
                Err(PaykitSdkError::Protocol { .. })
            ),
            "resolve accepted {uri:?}"
        );
    }
}

#[test]
fn test_parse_capabilities_rejects_invalid_entries() {
    assert_eq!(
        parse_capabilities(PAYKIT_SESSION_CAPABILITIES)
            .unwrap()
            .to_string(),
        PAYKIT_SESSION_CAPABILITIES
    );
    assert_eq!(PAYKIT_SESSION_CAPABILITIES, "/pub/paykit/:rw");
    assert!(parse_capabilities("/:rw,not-a-capability").is_err());
    assert!(parse_capabilities("/:rw,").is_err());
    assert!(parse_capabilities(",/:rw").is_err());
    assert!(parse_capabilities("").is_err());
}

#[test]
fn test_validate_auth_url_capabilities_requires_exact_match() {
    let auth_url = sign_in_auth_url(PAYKIT_SESSION_CAPABILITIES);

    assert!(validate_auth_url_capabilities(&auth_url, PAYKIT_SESSION_CAPABILITIES).is_ok());
    assert!(validate_auth_url_capabilities(&auth_url, "/:rw").is_err());
}

#[test]
fn test_validate_auth_url_capabilities_rejects_broad_capabilities() {
    let auth_url = sign_in_auth_url("/:rw");

    assert!(validate_auth_url_capabilities(&auth_url, PAYKIT_SESSION_CAPABILITIES).is_err());
}

#[tokio::test]
async fn test_approve_auth_rejects_mismatched_client_id_before_signup() {
    let bootstrap = PubkySessionBootstrap::new(TEST_CLIENT_ID).unwrap();
    let auth_url = sign_up_auth_url_for_client_id("/:rw", "attacker.test");

    let error = bootstrap
        .approve_auth(&auth_url, "/:rw", &PubkyLocalSecretKey::new([7; 32]))
        .await
        .unwrap_err();

    assert!(matches!(error, PaykitSdkError::Policy { .. }));
}

#[test]
fn test_validate_local_secret_for_public_key_rejects_mismatch() {
    let session_key = PubkyLocalSecretKey::new([7; 32]).public_key();
    let matching_secret = PubkyLocalSecretKey::new([7; 32]);
    let wrong_secret = PubkyLocalSecretKey::new([8; 32]);

    assert!(validate_local_secret_for_public_key(&session_key, matching_secret).is_ok());
    assert!(validate_local_secret_for_public_key(&session_key, wrong_secret).is_err());
}

#[test]
fn test_pubky_session_secret_debug_is_redacted() {
    let secret = PubkySessionSecret::new("pubky-session-secret".into());

    assert_eq!(format!("{secret:?}"), "PubkySessionSecret(<redacted>)");
    assert_eq!(secret.as_str(), "pubky-session-secret");
    assert_eq!(secret.into_inner(), "pubky-session-secret");
}

#[test]
fn test_pubky_auth_request_state_redacts_secrets() {
    let state =
        PubkyAuthRequestState::new(sign_in_auth_state_url("/:rw", &[42; 32]), [42; 32]).unwrap();
    let debug = format!("{state:?}");

    assert!(!debug.contains(TEST_AUTH_SECRET));
    assert!(!debug.contains("42"));
    assert_eq!(state.client_key_secret(), &[42; 32]);
}

#[test]
fn test_pubky_auth_request_state_rejects_mismatched_client_key() {
    let auth_url = sign_in_auth_state_url("/:rw", &[42; 32]);

    assert!(PubkyAuthRequestState::new(auth_url, [43; 32]).is_err());
}

#[tokio::test]
async fn test_pubky_auth_request_state_round_trips_shared_secret_grants() {
    let bootstrap = PubkySessionBootstrap::with_pubky(Pubky::new().unwrap(), TEST_CLIENT_ID)
        .unwrap()
        .with_auth_relay("http://127.0.0.1:1/inbox")
        .unwrap();
    for kind in [
        AuthFlowKind::signin(),
        AuthFlowKind::signup(
            PubkyLocalSecretKey::new([7; 32]).keypair().public_key(),
            None,
        ),
    ] {
        let request = bootstrap.start_auth("/:rw", kind).await.unwrap();
        let state = request.save_state().unwrap();
        assert!(state.to_grant_state().approval_key_secret.is_none());
        let serialized = serde_json::to_string(&state).unwrap();
        let restored: PubkyAuthRequestState = serde_json::from_str(&serialized).unwrap();
        let resumed = bootstrap.resume_auth(&restored, "/:rw").await.unwrap();
        assert_eq!(resumed.authorization_url(), request.authorization_url());
        assert_eq!(resumed.save_state().unwrap(), state);
    }
}

#[tokio::test]
async fn test_pubky_auth_helpers_reject_hpke_grants() {
    for kind in [
        AuthFlowKind::signin(),
        AuthFlowKind::signup(
            PubkyLocalSecretKey::new([7; 32]).keypair().public_key(),
            None,
        ),
    ] {
        let flow = PubkyGrantAuthFlow::builder(
            &parse_capabilities("/:rw").unwrap(),
            kind,
            parse_client_id(TEST_CLIENT_ID).unwrap(),
        )
        .approval_format(GrantApprovalFormat::SignedApprovalV1)
        .relay(Url::parse("http://127.0.0.1:1/inbox").unwrap())
        .start()
        .unwrap();
        let state = flow.save_local().unwrap();
        assert!(state.approval_key_secret.is_some());
        assert!(parse_pubky_auth_url(&state.authorization_url).is_err());
        assert!(
            PubkyAuthRequestState::new(state.authorization_url, state.client_key_secret).is_err()
        );
    }
}

#[test]
fn test_confirmed_inactive_grant_error_accepts_only_explicit_revocation_or_expiry() {
    for message in [
        "Grant has been revoked",
        "Grant has expired",
        "Invalid grant: grant has expired",
    ] {
        let error = pubky_server_error(401, message);
        assert!(is_confirmed_inactive_grant_error(&error));
    }

    assert!(!is_confirmed_inactive_grant_error(&pubky_server_error(
        404,
        "Grant not found",
    )));

    assert!(!is_confirmed_inactive_grant_error(
        &pubky::Error::Authentication(pubky::errors::AuthError::Validation(
            "stored grant credential has expired".into(),
        )),
    ));
}

#[test]
fn test_confirmed_inactive_grant_error_rejects_clock_skew_and_invalid_pop() {
    for message in [
        "Invalid PoP proof: PoP timestamp out of range",
        "Invalid PoP proof: invalid PoP signature",
        "PoP nonce already used",
        "Unauthorized",
    ] {
        let error = pubky_server_error(401, message);
        assert!(!is_confirmed_inactive_grant_error(&error));
    }
}

#[test]
fn test_signup_code_rejection_matches_homeserver_0_16_2_strings() {
    // Bodies from pubky-homeserver 0.16.2 `POST /auth/grant/signup`. Change
    // them only together with a homeserver upgrade that changes them.
    for (status, message) in [
        (400, "Token required"),
        (400, "Invalid signup token format: invalid length"),
        (401, "Invalid token"),
        (401, "Token already used"),
    ] {
        let error = pubky_server_error(status, message);
        assert!(is_signup_code_rejection(&error), "{status} {message}");
        assert!(map_pubky_signup_error("sign up Pubky identity", error).is_signup_code_rejected());
    }
}

#[test]
fn test_signup_code_rejection_rejects_other_server_errors() {
    for (status, message) in [
        // Canonical reasons that replace the body when the client keeps none.
        (400, "Bad Request"),
        (401, "Unauthorized"),
        // Clock skew and proof-of-possession failures, which a new code cannot fix.
        (400, "signup grant iat is too far in the future"),
        (401, "Invalid grant: grant has expired"),
        (401, "PoP nonce already used"),
        (409, "User already exists"),
        (404, "Not Found"),
        (401, "Token required"),
        (400, "Token requ\n[response body truncated at 10 bytes]"),
    ] {
        let error = pubky_server_error(status, message);
        assert!(!is_signup_code_rejection(&error), "{status} {message}");
        let mapped = map_pubky_signup_error("sign up Pubky identity", error);
        assert!(
            matches!(&mapped, PaykitSdkError::Identity { .. }) && !mapped.is_signup_code_rejected(),
            "{status} {message}: {mapped:?}"
        );
    }
}

#[tokio::test]
async fn test_map_pubky_signup_error_reports_retryable_failures_as_transport() {
    // Keep the loopback socket open without answering so the request times out.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let transport = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_millis(20))
        .build()
        .unwrap()
        .post(format!(
            "http://{}/auth/grant/signup?signup_token=SENTINEL_CODE",
            listener.local_addr().unwrap()
        ))
        .send()
        .await
        .unwrap_err();

    let retryable = [
        pubky::Error::Request(pubky::errors::RequestError::Transport(transport)),
        pubky_server_error(500, "Internal Server Error"),
        pubky_server_error(503, "Service Unavailable"),
        pubky_server_error(429, "Too Many Requests"),
        pubky::pkarr::errors::ResolveError::NoResponses.into(),
        pubky::pkarr::errors::PublishError::NoResponses.into(),
    ];
    for error in retryable {
        let rendered = error.to_string();
        let mapped = map_pubky_signup_error("sign up Pubky identity", error);
        assert!(
            matches!(
                &mapped,
                PaykitSdkError::Transport { context, .. } if context == "sign up Pubky identity"
            ),
            "{rendered}: {mapped:?}"
        );
        assert!(!mapped.to_string().contains("SENTINEL_CODE"), "{mapped}");
    }

    let not_retryable = [
        pubky::Error::Pkarr(pubky::errors::PkarrError::InvalidRecord(
            "_pubky target is not a public key".into(),
        )),
        pubky::Error::Request(pubky::errors::RequestError::Validation {
            message: "could not resolve homeserver".into(),
        }),
    ];
    for error in not_retryable {
        let rendered = error.to_string();
        let mapped = map_pubky_signup_error("sign up Pubky identity", error);
        assert!(
            matches!(&mapped, PaykitSdkError::Identity { .. }) && !mapped.is_signup_code_rejected(),
            "{rendered}: {mapped:?}"
        );
    }

    // Sign-in completion gets the Transport category but never the signup cause.
    let mapped = map_pubky_retryable_error(
        "complete Pubky auth flow",
        pubky_server_error(401, "Invalid token"),
    );
    assert!(
        matches!(&mapped, PaykitSdkError::Identity { .. }) && !mapped.is_signup_code_rejected(),
        "{mapped:?}"
    );
}

fn pubky_server_error(status: u16, message: &str) -> pubky::Error {
    pubky::Error::Request(pubky::errors::RequestError::Server {
        status: status.try_into().unwrap(),
        message: message.into(),
    })
}

mod signup_grant_approval;
