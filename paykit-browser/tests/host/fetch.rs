//! `FetchTransport`: the checks it makes before it calls `fetch`. Every request here is
//! refused before `fetch`, so none reaches the network; the hosts are under `.invalid`, which
//! never resolves, or a top-level name that does not exist, in case one did.

use paykit_browser::fetch::{FetchConfig, FetchTransport};
use pubky_switch_client::transport::{Header, Request, TimeoutClass, Transport, TransportError};
use wasm_bindgen_test::wasm_bindgen_test;

fn request(origin: &str) -> Request {
    Request {
        method: "GET".into(),
        origin: origin.into(),
        path_and_query: "/v1/x/directory".into(),
        headers: Vec::new(),
        body: Vec::new(),
        timeout: TimeoutClass::Ordinary,
    }
}

/// A config that allows port 8443, so an origin on it is refused only for its host.
fn config() -> FetchConfig {
    let mut config = FetchConfig::default();
    config.extra_ports = vec![8443];
    config
}

/// A request that passes the origin checks and then fails the header check, before `fetch`:
/// a header `fetch` forbids.
fn with_forbidden_header(origin: &str) -> Request {
    let mut request = request(origin);
    request.headers.push(Header::new("host", "switch.invalid"));
    request
}

/// The defaults are those of the native transport.
#[wasm_bindgen_test]
fn test_fetch_config_defaults_match_the_native_transport() {
    let config = FetchConfig::default();
    assert_eq!(config.request_timeout_ms, 10_000);
    assert_eq!(config.long_timeout_ms, 40_000);
    assert!(config.extra_ports.is_empty());
    assert_eq!(config.max_response_bytes, 32 << 20);
}

/// Origins outside the Switch's rules, and the names of the local machine in every spelling,
/// are refused before `fetch` (Switch wire 7.1, 8.2).
#[wasm_bindgen_test]
async fn test_fetch_refuses_origins_before_sending() {
    let transport = FetchTransport::new(config());
    for origin in [
        "http://switch.invalid",
        "https://127.0.0.1",
        "https://[::1]",
        "https://switch.invalid:6379",
        "https://user@switch.invalid",
        "https://switch.invalid/path",
        "https://localhost:8443",
        "https://LOCALHOST",
        "https://localhost.",
        "https://inbox.localhost",
        "https://Inbox.LocalHost:8443",
        "https://localhost.localdomain",
        "https://LocalHost.LocalDomain:8443",
        "https://localhost6.localdomain6",
        "https://localdomain",
        "https://switch.localdomain",
    ] {
        let error = transport.send(request(origin)).await.unwrap_err();
        assert!(
            matches!(error, TransportError::Refused(_)),
            "{origin}: {error:?}"
        );
    }
    // An origin on an allowed port passes the origin checks: the header check refuses it.
    let error = transport
        .send(with_forbidden_header("https://switch.invalid:8443"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, TransportError::InvalidRequest(_)),
        "{error:?}"
    );
}

/// Host names browsers resolve on the local network are refused before `fetch`, by both
/// constructors, in any case: a name of one label, and `local`, `internal`, `home.arpa`,
/// `lan`, `home`, `corp`, `private` and `intranet` and the names under them (Switch wire 8.2).
#[wasm_bindgen_test]
async fn test_fetch_refuses_local_network_names_before_sending() {
    let local_names = [
        "https://router",
        "https://Router:8443",
        "https://router.",
        "https://local",
        "https://printer.local",
        "https://Printer.LOCAL",
        "https://switch.internal",
        "https://internal",
        "https://home.arpa",
        "https://switch.home.arpa",
        "https://switch.Home.Arpa:8443",
        "https://switch.lan",
        "https://lan",
        "https://router.home",
        "https://Router.Home:8443",
        "https://home",
        "https://nas.corp",
        "https://switch.private",
        "https://intranet",
        "https://wiki.intranet",
    ];
    for transport in [
        FetchTransport::new(config()),
        FetchTransport::allowing_localhost_for_loopback_tests(config()),
    ] {
        for origin in local_names {
            let error = transport.send(request(origin)).await.unwrap_err();
            assert!(
                matches!(error, TransportError::Refused(_)),
                "{origin}: {error:?}"
            );
        }
        // A suffix that only ends a label, a suffix as an inner label, and a public IPv6
        // literal, which has no dot, pass the origin checks: the header check refuses them.
        for origin in [
            "https://switch.xlan",
            "https://switch.myhome.arpa",
            "https://local.switch.invalid",
            "https://switch.homes.example",
            "https://switch.xcorp",
            "https://home.switch.invalid",
            "https://[2606:4700::1111]:8443",
        ] {
            let error = transport
                .send(with_forbidden_header(origin))
                .await
                .unwrap_err();
            assert!(
                matches!(error, TransportError::InvalidRequest(_)),
                "{origin}: {error:?}"
            );
        }
    }
}

/// The loopback test transport allows the names of the local machine, but still only over
/// `https`, on allowed ports, and never to a private address literal.
#[wasm_bindgen_test]
async fn test_fetch_for_loopback_tests_allows_only_localhost() {
    let transport = FetchTransport::allowing_localhost_for_loopback_tests(config());
    for origin in [
        "http://localhost:8443",
        "https://localhost:9999",
        "https://127.0.0.1:8443",
        "https://[::1]:8443",
        "http://localhost.localdomain",
    ] {
        let error = transport.send(request(origin)).await.unwrap_err();
        assert!(
            matches!(error, TransportError::Refused(_)),
            "{origin}: {error:?}"
        );
    }
    // The names of the local machine pass the origin checks: the header check refuses the
    // request.
    for origin in [
        "https://localhost:8443",
        "https://inbox.localhost",
        "https://localhost.localdomain",
        "https://localhost6.localdomain6:8443",
    ] {
        let error = transport
            .send(with_forbidden_header(origin))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::InvalidRequest(_)),
            "{origin}: {error:?}"
        );
    }
}

/// A request `fetch` would not send as signed is refused before `fetch`: a path or query the
/// URL parser changes, a fragment, a header `fetch` forbids, trims, merges or refuses, a
/// method it refuses, and a body on a `GET` (Switch wire 7.1).
#[wasm_bindgen_test]
async fn test_fetch_refuses_requests_it_cant_send_as_signed() {
    let transport = FetchTransport::new(FetchConfig::default());
    let origin = "https://switch.invalid";
    let mut cases = Vec::new();
    for path in [
        "v1/x/directory",
        "/v1/x/../y",
        "/v1/x directory",
        "/v1/x/feed?after=a b",
        "/v1/x/directory#part",
    ] {
        let mut request = request(origin);
        request.path_and_query = path.into();
        cases.push(request);
    }
    for headers in [
        vec![Header::new("host", "switch.invalid")],
        vec![Header::new("cookie", "a=b")],
        vec![Header::new("sec-anything", "a")],
        vec![Header::new("proxy-anything", "a")],
        vec![Header::new("switch-identity", " padded")],
        vec![Header::new("switch-identity", "a\nb")],
        vec![Header::new("switch-identity", "caf\u{e9}")],
        vec![Header::new("bad name", "a")],
        vec![
            Header::new("switch-identity", "a"),
            Header::new("Switch-Identity", "b"),
        ],
    ] {
        let mut request = request(origin);
        request.headers = headers;
        cases.push(request);
    }
    for method in ["TRACE", "CONNECT", "BAD METHOD"] {
        let mut request = request(origin);
        request.method = method.into();
        cases.push(request);
    }
    let mut get_with_body = request(origin);
    get_with_body.body = b"{}".to_vec();
    cases.push(get_with_body);
    for case in cases {
        let error = transport.send(case.clone()).await.unwrap_err();
        assert!(
            matches!(error, TransportError::InvalidRequest(_)),
            "{} {} {:?}: {error:?}",
            case.method,
            case.path_and_query,
            case.headers
        );
    }
}
