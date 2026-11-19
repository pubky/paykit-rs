//! What the network test targets share: the rig's origin, and a `FetchTransport` and
//! requests that reach it (`rig/switch-rig.mjs`).
//!
//! `rig/test.sh --switch` starts a real Switch service behind the rig, on
//! `https://localhost:<rig port>`, and passes that origin to the build in
//! `PAYKIT_BROWSER_RIG_ORIGIN`. A network test target includes this module with
//! `#[path = "../switch_rig/mod.rs"]`; the `fetch_transport` and `switch_flow` targets both do.

use paykit_browser::fetch::{FetchConfig, FetchTransport};
use pubky_switch_client::transport::{Request, TimeoutClass};

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
