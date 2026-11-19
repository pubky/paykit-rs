//! What the network test targets share: the rig's origin, a `FetchTransport` that may reach
//! it, and helpers for the rig's own routes under `/rig/` (`rig/switch-rig.mjs`).
//!
//! `rig/test.sh --switch` starts a real Switch service behind the rig, on
//! `https://localhost:<rig port>`, and passes that origin to the build in
//! `PAYKIT_BROWSER_RIG_ORIGIN`. A network test target includes this module with
//! `#[path = "../switch_rig/mod.rs"]`.

use paykit_browser::fetch::{FetchConfig, FetchTransport};
use pubky_switch_client::transport::{Request, TimeoutClass, Transport};
use serde_json::Value;

/// The rig's origin, `https://localhost:<rig port>`, as `rig/test.sh --switch` set it at
/// compile time.
///
/// # Panics
///
/// When the target was built without it: a network test never passes without its rig.
pub fn origin() -> &'static str {
    option_env!("PAYKIT_BROWSER_RIG_ORIGIN").unwrap_or_else(|| {
        panic!(
            "PAYKIT_BROWSER_RIG_ORIGIN is not set: run the network tests with \
             `paykit-browser/rig/test.sh --switch`"
        )
    })
}

/// The rig's port, from [`origin`].
///
/// # Panics
///
/// When the origin is not `https://localhost:<port>`.
pub fn port() -> u16 {
    origin()
        .strip_prefix("https://localhost:")
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "PAYKIT_BROWSER_RIG_ORIGIN must be https://localhost:<port>, as \
                 `rig/test.sh --switch` sets it"
            )
        })
}

/// The default settings, with the rig's port allowed.
pub fn config() -> FetchConfig {
    let mut config = FetchConfig::default();
    config.extra_ports = vec![port()];
    config
}

/// A transport that may reach the rig: the loopback test transport, on [`config`].
pub fn transport() -> FetchTransport {
    FetchTransport::allowing_localhost_for_loopback_tests(config())
}

/// A request to the rig, with no headers and no body, and the ordinary timeout class.
pub fn request(method: &str, path_and_query: &str) -> Request {
    Request {
        method: method.into(),
        origin: origin().into(),
        path_and_query: path_and_query.into(),
        headers: Vec::new(),
        body: Vec::new(),
        timeout: TimeoutClass::Ordinary,
    }
}

/// A tag no other request to this rig has used: `name`, the wall clock and a random number.
/// One rig serves every engine and context of a run, so a tag must differ between them too.
/// It holds only letters, digits and `-`, so it goes into a query as it is.
pub fn fresh_tag(name: &str) -> String {
    let now = js_sys::Date::now() as u64;
    let random = (js_sys::Math::random() * f64::from(u32::MAX)) as u32;
    format!("{name}-{now}-{random:08x}")
}

/// What the rig has seen, from `/rig/count`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Count {
    /// The requests whose query has the tag, preflights included.
    pub requests: u64,
    /// The failed TLS handshakes since the rig started, from any client. Browsers make some
    /// of their own: compare two readings around a probe, never a reading with zero.
    pub tls_errors: u64,
}

/// What the rig has seen for `tag`, read through [`transport`].
///
/// # Panics
///
/// When the rig does not answer, or answers something other than its count.
pub async fn count(tag: &str) -> Count {
    let response = transport()
        .send(request("GET", &format!("/rig/count?tag={tag}")))
        .await
        .expect("the rig answers /rig/count");
    assert_eq!(response.status, 200, "/rig/count");
    let value: Value = serde_json::from_slice(&response.body).expect("/rig/count answers JSON");
    let field = |name: &str| {
        value[name]
            .as_u64()
            .unwrap_or_else(|| panic!("/rig/count answers no {name}"))
    };
    Count {
        requests: field("requests"),
        tls_errors: field("tls_errors"),
    }
}
