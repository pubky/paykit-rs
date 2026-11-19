//! `FetchTransport` over a real network, in a browser: the rig's fixed routes on
//! `https://localhost:<rig port>`, which `rig/test.sh --switch` serves with TLS and CORS
//! (`rig/switch-rig.mjs`).
//!
//! Each test sends through the transport's `send`, as the Switch client does, and checks what
//! comes back. The routes that count requests by a `tag` in their query show that a refused
//! request, and a redirect's target, never reached the rig.
//!
//! The transport exists only on `wasm32-unknown-unknown`, so natively the target is empty.
//! wasm-bindgen-test runs it in a browser page or in a dedicated worker; the runner's
//! environment picks the browser and the context (`rig/runner.mjs`).

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

#[path = "../switch_rig/mod.rs"]
mod switch_rig;

use paykit_browser::fetch::{FetchConfig, FetchTransport};
use pubky_switch_client::transport::{
    header_value, Header, Request, Response, Transport, TransportError,
};
use pubky_switch_core::enc::b64;
use serde_json::Value;
use wasm_bindgen_test::wasm_bindgen_test;

use switch_rig::{config, origin, port, request, transport};

/// A loopback port nothing listens on, as `rig/test.sh --switch` checked on `127.0.0.1` and
/// `::1` and set at compile time in `PAYKIT_BROWSER_RIG_CLOSED_PORT`: 18449 unless that
/// variable was set for the run.
///
/// # Panics
///
/// When the target was built without it, or with something other than a port number.
fn closed_port() -> u16 {
    option_env!("PAYKIT_BROWSER_RIG_CLOSED_PORT")
        .unwrap_or_else(|| {
            panic!(
                "PAYKIT_BROWSER_RIG_CLOSED_PORT is not set: run the network tests with \
                 `paykit-browser/rig/test.sh --switch`"
            )
        })
        .parse()
        .expect("PAYKIT_BROWSER_RIG_CLOSED_PORT is a port number")
}

/// The bytes `/rig/bytes?n=<n>&seed=<seed>` answers: byte `i` is `(i * 31 + seed) mod 256`.
fn rig_bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| ((i * 31 + usize::from(seed)) % 256) as u8)
        .collect()
}

/// A tag no other request to this rig has used: `name`, the wall clock and a random number.
/// One rig serves every engine and context of a run, so a tag must differ between them too.
/// It holds only letters, digits and `-`, so it goes into a query as it is.
fn fresh_tag(name: &str) -> String {
    let now = js_sys::Date::now() as u64;
    let random = (js_sys::Math::random() * f64::from(u32::MAX)) as u32;
    format!("{name}-{now}-{random:08x}")
}

/// The requests the rig has seen whose query has `tag`, preflights included, read from
/// `/rig/count` through [`transport`].
///
/// # Panics
///
/// When the rig does not answer, or answers something other than its count.
async fn count(tag: &str) -> u64 {
    let response = transport()
        .send(request("GET", &format!("/rig/count?tag={tag}")))
        .await
        .expect("the rig answers /rig/count");
    assert_eq!(response.status, 200, "/rig/count");
    let value: Value = serde_json::from_slice(&response.body).expect("/rig/count answers JSON");
    value["requests"]
        .as_u64()
        .expect("/rig/count answers its requests")
}

/// The request `/rig/echo` received, from its answer.
struct Echo {
    method: String,
    path: String,
    /// The headers in the order received, names in lower case.
    headers: Vec<Header>,
    /// The body, base64url without padding (Switch wire 1.2).
    body_b64: String,
}

fn echo(response: &Response) -> Echo {
    assert_eq!(response.status, 200, "/rig/echo");
    let value: Value = serde_json::from_slice(&response.body).expect("/rig/echo answers JSON");
    let text = |name: &str| {
        value[name]
            .as_str()
            .unwrap_or_else(|| panic!("/rig/echo answers no {name}"))
            .to_owned()
    };
    let headers = value["headers"]
        .as_array()
        .expect("/rig/echo answers its headers")
        .iter()
        .map(|pair| {
            let pair = pair.as_array().expect("a header is a pair");
            let part = |i: usize| pair[i].as_str().expect("a header is text");
            Header::new(part(0), part(1))
        })
        .collect();
    Echo {
        method: text("method"),
        path: text("path"),
        headers,
        body_b64: text("body_b64"),
    }
}

/// Sends `request` through `transport` and checks that it is refused before `fetch`: the
/// error is `Refused`, and the rig saw no request with `tag`.
async fn assert_refused_unsent(transport: &FetchTransport, request: Request, tag: &str) {
    let url = request.url();
    let error = transport.send(request).await.unwrap_err();
    assert!(
        matches!(error, TransportError::Refused(_)),
        "{url}: {error:?}"
    );
    assert_eq!(count(tag).await, 0, "{url} reached the rig");
}

/// A `GET` returns the status, the headers the rig exposes and the exact body, with a
/// `content-length` and streamed without one, across several chunks.
#[wasm_bindgen_test]
async fn test_fetch_round_trip_get_returns_the_exact_body() {
    let transport = transport();
    for (seed, chunked) in [(7u8, ""), (201, "&chunked=1")] {
        let n = 200_000;
        let path = format!("/rig/bytes?n={n}&seed={seed}{chunked}");
        let response = transport.send(request("GET", &path)).await.unwrap();
        assert_eq!(response.status, 200, "{path}");
        assert_eq!(
            response.header("x-rig-fixed"),
            Some("rig-fixed-value"),
            "{path}"
        );
        assert!(
            response.body == rig_bytes(n, seed),
            "{path}: a body of {} bytes differs",
            response.body.len()
        );
    }
}

/// A `POST` with every byte value in its body arrives byte for byte, at the path and query
/// sent.
#[wasm_bindgen_test]
async fn test_fetch_round_trip_post_sends_the_exact_body() {
    let body: Vec<u8> = (0..=255).collect();
    let path = format!("/rig/echo?tag={}", fresh_tag("post"));
    let mut post = request("POST", &path);
    post.headers
        .push(Header::new("content-type", "application/octet-stream"));
    post.body = body.clone();
    let echo = echo(&transport().send(post).await.unwrap());
    assert_eq!(echo.method, "POST");
    assert_eq!(echo.path, path);
    assert_eq!(echo.body_b64, b64(&body));
}

/// The Switch's request headers and the body's type arrive with the values sent, once each,
/// with the method, path and query sent. Headers the browser adds are not checked.
#[wasm_bindgen_test]
async fn test_fetch_request_headers_arrive_unchanged() {
    let sent = [
        (
            "switch-identity",
            "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo",
        ),
        ("switch-app", "paykit-app-1"),
        ("switch-cert", "eyJhbGciOiJFZERTQSJ9.e30.c2lnbmF0dXJl"),
        ("switch-invite-code", "INVITE-0123-abcd"),
        ("switch-transfer", "t_9f8e7d6c5b4a"),
        ("switch-timestamp", "1763593200000"),
        (
            "switch-signature",
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8+/w==",
        ),
        ("switch-created", "1763590000123"),
        ("content-type", "application/json"),
    ];
    let path = format!("/rig/echo?tag={}&after=a%2Fb~c", fresh_tag("headers"));
    let mut put = request("PUT", &path);
    put.headers = sent
        .iter()
        .map(|(name, value)| Header::new(name, value))
        .collect();
    put.body = b"{}".to_vec();
    let echo = echo(&transport().send(put).await.unwrap());
    assert_eq!(echo.method, "PUT");
    assert_eq!(echo.path, path);
    assert_eq!(echo.body_b64, b64(b"{}"));
    // A header that arrived twice reads as absent.
    for (name, value) in sent {
        assert_eq!(header_value(&echo.headers, name), Some(value), "{name}");
    }
}

/// An answer with a status outside 2xx is returned as an answer, not as an error.
#[wasm_bindgen_test]
async fn test_fetch_error_status_is_a_response() {
    let transport = transport();
    for code in [404u16, 503] {
        let response = transport
            .send(request("GET", &format!("/rig/status?code={code}")))
            .await
            .unwrap();
        assert_eq!(response.status, code);
        assert_eq!(response.body, format!("status {code}").into_bytes());
    }
}

/// A redirect answer is an error, `Other`, and its target is never requested: the rig sees
/// the redirect request only.
#[wasm_bindgen_test]
async fn test_fetch_redirect_is_an_error_and_not_followed() {
    let tag = fresh_tag("redirect");
    let error = transport()
        .send(request("GET", &format!("/rig/redirect?tag={tag}")))
        .await
        .unwrap_err();
    assert!(matches!(error, TransportError::Other(_)), "{error:?}");
    assert_eq!(count(&tag).await, 1);
}

/// An answer body over `max_response_bytes` is `TooLarge` with that limit, counted as it
/// streams, with a `content-length` and without one; a body of exactly the limit is read.
#[wasm_bindgen_test]
async fn test_fetch_body_over_the_limit_is_too_large() {
    let limit = 1_000;
    let mut small = config();
    small.max_response_bytes = limit;
    let transport = FetchTransport::allowing_localhost_for_loopback_tests(small);
    for chunked in ["", "&chunked=1"] {
        let over = format!("/rig/bytes?n=5000&seed=3{chunked}");
        let error = transport.send(request("GET", &over)).await.unwrap_err();
        assert_eq!(error, TransportError::TooLarge(limit), "{over}");

        let at = format!("/rig/bytes?n={limit}&seed=3{chunked}");
        let response = transport.send(request("GET", &at)).await.unwrap();
        assert_eq!(response.body, rig_bytes(1_000, 3), "{at}");
    }
}

/// An answer that announces a `content-length` and breaks off after 16 bytes is `Other`, an
/// unknown outcome, also when the length is beyond what a 32-bit WebAssembly instance can
/// hold, and when it is over the limit: the announced length sizes no buffer, and the limit
/// counts only the bytes read.
#[wasm_bindgen_test]
async fn test_fetch_body_shorter_than_announced_is_other() {
    for (limit, length) in [
        (u64::MAX, 1_000u64),
        (u64::MAX, 3_000_000_000),
        (100, 1_000),
    ] {
        let mut limited = config();
        limited.max_response_bytes = limit;
        let transport = FetchTransport::allowing_localhost_for_loopback_tests(limited);
        let path = format!("/rig/truncated?length={length}");
        let error = transport.send(request("GET", &path)).await.unwrap_err();
        assert!(
            matches!(error, TransportError::Other(_)),
            "{path} with limit {limit}: {error:?}"
        );
    }
}

/// The answer to a `HEAD` is returned, with no body, though its `content-length` announces
/// more than the limit: the limit counts the bytes read, not the length announced.
#[wasm_bindgen_test]
async fn test_fetch_head_answer_is_not_held_to_its_announced_length() {
    let limit = 10;
    let mut small = config();
    small.max_response_bytes = limit;
    let response = FetchTransport::allowing_localhost_for_loopback_tests(small)
        .send(request("HEAD", "/rig/echo"))
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    let announced: u64 = response
        .header("content-length")
        .and_then(|length| length.parse().ok())
        .expect("the answer to a HEAD announces its length");
    assert!(announced > limit, "content-length {announced}");
    assert!(response.body.is_empty());
}

/// An origin the transport refuses is `Refused`, and nothing reaches the rig, though each
/// one points at the rig's own port: plain `http`, a port that is not allowed, an address
/// literal, and `localhost` through `FetchTransport::new`.
#[wasm_bindgen_test]
async fn test_fetch_refused_origins_send_nothing() {
    let port = port();
    let echo_request = |origin: String, tag: &str| {
        let mut request = request("GET", &format!("/rig/echo?tag={tag}"));
        request.origin = origin;
        request
    };

    let tag = fresh_tag("refused-http");
    assert_refused_unsent(
        &transport(),
        echo_request(format!("http://localhost:{port}"), &tag),
        &tag,
    )
    .await;

    let tag = fresh_tag("refused-port");
    assert_refused_unsent(
        &FetchTransport::allowing_localhost_for_loopback_tests(FetchConfig::default()),
        echo_request(origin().into(), &tag),
        &tag,
    )
    .await;

    let tag = fresh_tag("refused-address");
    assert_refused_unsent(
        &transport(),
        echo_request(format!("https://127.0.0.1:{port}"), &tag),
        &tag,
    )
    .await;

    let tag = fresh_tag("refused-localhost");
    assert_refused_unsent(
        &FetchTransport::new(config()),
        echo_request(origin().into(), &tag),
        &tag,
    )
    .await;
}

/// An origin that does not answer is `Other`, an unknown outcome, never `Connect` or
/// `Refused`, which would tell the client nothing was sent.
#[wasm_bindgen_test]
async fn test_fetch_unreachable_origin_is_other() {
    let closed_port = closed_port();
    let mut closed = config();
    closed.extra_ports.push(closed_port);
    let mut unreachable = request("GET", "/rig/health");
    unreachable.origin = format!("https://localhost:{closed_port}");
    let error = FetchTransport::allowing_localhost_for_loopback_tests(closed)
        .send(unreachable)
        .await
        .unwrap_err();
    assert!(matches!(error, TransportError::Other(_)), "{error:?}");
}

/// A call past its timeout is `Timeout`, as soon as the timeout passes rather than when the
/// slow answer comes.
#[wasm_bindgen_test]
async fn test_fetch_call_past_its_timeout_is_timeout() {
    let mut short = config();
    short.request_timeout_ms = 300;
    let transport = FetchTransport::allowing_localhost_for_loopback_tests(short);
    let started = js_sys::Date::now();
    let error = transport
        .send(request("GET", "/rig/slow?ms=3000"))
        .await
        .unwrap_err();
    let elapsed = js_sys::Date::now() - started;
    assert_eq!(error, TransportError::Timeout);
    assert!(
        (250.0..2_000.0).contains(&elapsed),
        "returned after {elapsed} ms"
    );
}
