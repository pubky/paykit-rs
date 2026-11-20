//! What the tests of this target share: requests with a Switch header, a `fetch` in the
//! browser's default cache mode, and the check that the browser closed a call's connection.

use js_sys::{Function, Promise, Reflect};
use pubky_switch_client::transport::{Header, Request};
use pubky_switch_core::http::header;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, RequestCredentials, RequestInit, RequestMode};

use crate::switch_rig::wait::{now_ms, wait_until};
use crate::switch_rig::{origin, request, tag_count};

/// A request with a Switch request header, `switch-timestamp`, as every signed request of the
/// Switch client carries (Switch wire 7.1). A header outside the CORS-safelisted ones makes the
/// browser send a preflight before the request, unless it holds a fresh one for the URL.
pub fn preflighted(method: &str, path_and_query: &str) -> Request {
    let mut request = request(method, path_and_query);
    request
        .headers
        .push(Header::new(header::TIMESTAMP, SWITCH_TIMESTAMP));
    request
}

/// The value [`preflighted`] gives its Switch header.
pub const SWITCH_TIMESTAMP: &str = "1763593200000";

/// Calls the global `fetch`, which pages and dedicated workers both have, with `request`. It
/// is looked up at run time: a second binding of the same import as the transport's would
/// clash with it at link time.
fn global_fetch(request: &web_sys::Request) -> Option<Promise> {
    let global = js_sys::global();
    let fetch: Function = Reflect::get(&global, &JsValue::from_str("fetch"))
        .ok()?
        .dyn_into()
        .ok()?;
    fetch.call1(&global, request).ok()?.dyn_into().ok()
}

/// A `GET` to `path_and_query` on the rig with `headers`, through the global `fetch` with the
/// browser's default cache mode, as a page's own `fetch` would send it, not through the
/// transport: the answer's body as text, or `None` when the call fails. It shows what the
/// browser does where the transport's `cache: "no-store"` does not apply.
pub async fn default_mode_get(path_and_query: &str, headers: &[(&str, &str)]) -> Option<String> {
    let sent = Headers::new().ok()?;
    for (name, value) in headers {
        sent.append(name, value).ok()?;
    }
    let init = RequestInit::new();
    init.set_method("GET");
    init.set_headers(&sent);
    init.set_mode(RequestMode::Cors);
    init.set_credentials(RequestCredentials::Omit);
    let url = format!("{}{path_and_query}", origin());
    let built = web_sys::Request::new_with_str_and_init(&url, &init).ok()?;
    let response: web_sys::Response = JsFuture::from(global_fetch(&built)?)
        .await
        .ok()?
        .dyn_into()
        .ok()?;
    JsFuture::from(response.text().ok()?)
        .await
        .ok()?
        .as_string()
}

/// Waits up to `bound_ms` milliseconds for the rig to see the one call with `tag` closed by
/// the browser, with nothing left open, checks that it did, and returns how long it took,
/// in milliseconds. `case` names the call in a failure.
///
/// # Panics
///
/// When the rig shows anything but one call, closed early by the client, and none open.
pub async fn assert_closed_by_browser(case: &str, tag: &str, bound_ms: u64) -> f64 {
    let started = now_ms();
    let count = wait_until(
        bound_ms,
        || tag_count(tag),
        |count| count.open == 0 && count.closed_early > 0,
    )
    .await;
    let closed_after = now_ms() - started;
    assert_eq!(
        (count.calls(), count.closed_early, count.open),
        (1, 1, 0),
        "{case}: the rig did not see the one call closed by the browser: {count:?}"
    );
    closed_after
}
