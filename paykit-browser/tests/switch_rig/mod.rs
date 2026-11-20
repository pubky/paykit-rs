//! What the network test targets share: the rig's origin, a `FetchTransport` and requests that
//! reach it, and the rig's own routes and counters (`rig/switch-rig.mjs`); bounded waits are in
//! [`wait`]. Each target uses only some of these, so each includes this module with
//! `#[allow(dead_code)]`.
//!
//! `rig/test.sh --switch` starts a real Switch service behind the rig, on
//! `https://localhost:<rig port>`, and passes that origin to the build in
//! `PAYKIT_BROWSER_RIG_ORIGIN`. A network test target includes this module with
//! `#[path = "../switch_rig/mod.rs"]`; the `fetch_transport` and `switch_flow` targets both do.

pub mod wait;

use paykit_browser::fetch::{FetchConfig, FetchTransport};
use pubky_switch_client::transport::{Header, Request, Response, TimeoutClass, Transport};
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

/// The token the rig requires for the routes that change it (`POST /rig/world` and
/// `/rig/fault`), which `rig/test.sh --switch` makes for each run and sets at compile time:
/// the rig refuses a request without it, so a page or program that does not know it cannot
/// make the rig enrol or fault.
///
/// # Panics
///
/// When the target was built without it: a network test never passes without its rig.
pub fn rig_token() -> &'static str {
    option_env!("PAYKIT_BROWSER_RIG_TOKEN").unwrap_or_else(|| {
        panic!(
            "PAYKIT_BROWSER_RIG_TOKEN is not set: run the network tests with \
             `paykit-browser/rig/test.sh --switch`"
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
    transport_with(|_| {})
}

/// A transport that may reach the rig, on [`config`] changed by `edit`. The production
/// defaults (`FetchConfig::default`) are left as they are.
pub fn transport_with(edit: impl FnOnce(&mut FetchConfig)) -> FetchTransport {
    let mut config = config();
    edit(&mut config);
    FetchTransport::allowing_localhost_for_loopback_tests(config)
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
/// It holds only letters, digits and `-`, so it goes into a query or a path as it is.
pub fn fresh_tag(name: &str) -> String {
    let now = js_sys::Date::now() as u64;
    let random = (js_sys::Math::random() * f64::from(u32::MAX)) as u32;
    format!("{name}-{now}-{random:08x}")
}

/// The bytes `/rig/bytes?n=<n>&seed=<seed>` answers: byte `i` is `(i * 31 + seed) mod 256`.
pub fn rig_bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| ((i * 31 + usize::from(seed)) % 256) as u8)
        .collect()
}

/// Sends `method` to the rig route `path_and_query` through [`transport`], with `body` as
/// JSON when given, and returns its JSON answer. A `GET` carries no header, so it needs no
/// CORS preflight; any other method, and a JSON body, make the browser send one first, which
/// the rig answers itself.
///
/// # Panics
///
/// When the rig does not answer `200` with JSON.
pub async fn rig_json(method: &str, path_and_query: &str, body: Option<Value>) -> Value {
    let mut call = request(method, path_and_query);
    if let Some(body) = body {
        call.headers
            .push(Header::new("content-type", "application/json"));
        call.body = serde_json::to_vec(&body).expect("the body is JSON");
    }
    let response = transport()
        .send(call)
        .await
        .unwrap_or_else(|error| panic!("the rig answers {method} {path_and_query}: {error:?}"));
    assert_eq!(
        response.status,
        200,
        "{method} {path_and_query}: {}",
        String::from_utf8_lossy(&response.body)
    );
    serde_json::from_slice(&response.body)
        .unwrap_or_else(|_| panic!("{method} {path_and_query} answers no JSON"))
}

/// Disarms the rig's fault (`DELETE /rig/fault`), if one is armed. The rig serves every
/// target of a run, and a test whose call panicked leaves its fault armed. Such a fault
/// matches only that test's path, but while it is armed the rig closes every browser
/// connection after its answer. A test that arms a fault, or measures connections or
/// preflights on a path the rig forwards, calls this first.
///
/// # Panics
///
/// When the rig does not answer that no fault is armed.
pub async fn disarm_fault() {
    let state = rig_json("DELETE", &format!("/rig/fault?token={}", rig_token()), None).await;
    assert_eq!(
        state,
        serde_json::json!({ "armed": false }),
        "DELETE /rig/fault"
    );
}

/// The count `name` in a JSON answer of the rig `route`.
///
/// # Panics
///
/// When the answer has no such count.
fn number(value: &Value, route: &str, name: &str) -> u64 {
    value[name]
        .as_u64()
        .unwrap_or_else(|| panic!("{route} answers no {name}"))
}

/// What the rig counted for one tag (`/rig/count`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagCount {
    /// The requests whose query has the tag, preflights included.
    pub requests: u64,
    /// The `OPTIONS` requests among them.
    pub preflights: u64,
    /// The other requests whose connection the client closed before the rig finished its
    /// answer.
    pub closed_early: u64,
    /// The other requests still in progress.
    pub open: u64,
}

impl TagCount {
    /// The requests that were not preflights.
    pub fn calls(&self) -> u64 {
        self.requests.saturating_sub(self.preflights)
    }
}

/// What the rig counted for `tag` so far. The request to `/rig/count` is not counted under
/// any tag.
///
/// # Panics
///
/// When the rig does not answer its count.
pub async fn tag_count(tag: &str) -> TagCount {
    let value = rig_json("GET", &format!("/rig/count?tag={tag}"), None).await;
    let count = |name| number(&value, "/rig/count", name);
    TagCount {
        requests: count("requests"),
        preflights: count("preflights"),
        closed_early: count("closed_early"),
        open: count("open"),
    }
}

/// What the rig counted for one path it forwards to the service (`/rig/hits`), for one
/// method or all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathHits {
    /// The `OPTIONS` requests to the path, of any method, which the rig answers itself.
    pub preflights: u64,
    /// The other requests to the path the rig sent on to the service, a lost or held one
    /// included once it is sent. A refused request, and a held one whose client gave up
    /// before the hold ended, never are.
    pub forwarded: u64,
    /// The other requests to the path a `refuse` fault kept from the service.
    pub refused: u64,
    /// The other requests to the path still in progress, sent on or not.
    pub open: u64,
}

impl PathHits {
    /// The requests counted since `earlier`, a count of the same path and method read before;
    /// `open` stays the one of `self`, the requests in progress now.
    pub fn since(self, earlier: PathHits) -> PathHits {
        PathHits {
            preflights: self.preflights - earlier.preflights,
            forwarded: self.forwarded - earlier.forwarded,
            refused: self.refused - earlier.refused,
            open: self.open,
        }
    }
}

/// The `GET /rig/hits` request for the forwarded `path`, a raw path without query, counting
/// only `method` when one is given. It carries no header, so it needs no preflight: it is an
/// ordinary call to the rig's origin.
pub fn hits_request(path: &str, method: Option<&str>) -> Request {
    let encoded = String::from(js_sys::encode_uri_component(path));
    let method = method.map_or(String::new(), |method| format!("&method={method}"));
    request("GET", &format!("/rig/hits?path={encoded}{method}"))
}

/// The counts in the rig's answer to a [`hits_request`].
///
/// # Panics
///
/// When the answer is not a 200 with the four counts.
pub fn read_hits(response: &Response) -> PathHits {
    assert_eq!(response.status, 200, "/rig/hits");
    let value: Value = serde_json::from_slice(&response.body).expect("/rig/hits answers JSON");
    let count = |name| number(&value, "/rig/hits", name);
    PathHits {
        preflights: count("preflights"),
        forwarded: count("forwarded"),
        refused: count("refused"),
        open: count("open"),
    }
}

/// What the rig counted so far for the forwarded `path`, a raw path without query, counting
/// only `method` when one is given.
///
/// # Panics
///
/// When the rig does not answer its count.
pub async fn path_hits(path: &str, method: Option<&str>) -> PathHits {
    let response = transport()
        .send(hits_request(path, method))
        .await
        .unwrap_or_else(|error| panic!("/rig/hits: {error:?}"));
    read_hits(&response)
}
