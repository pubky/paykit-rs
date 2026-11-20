//! The Switch client's transport in a browser: [`FetchTransport`], on the browser's `fetch`.
//!
//! A switch origin comes from an `identity.json` someone else controls, so the transport
//! checks every origin before it calls `fetch` (Switch wire 7.1), and refuses the names of the
//! local machine, such as `localhost`, and the names browsers resolve on the local network, since a browser gives it no way to check
//! the addresses a host name resolves to (Switch wire 8.2). Redirects are not followed. A path or query the browser's URL parser would
//! change, and a header `fetch` would drop or change, are refused before anything is sent:
//! the request must go out as the exact bytes the client signed (Switch wire 7.1).
//!
//! The error mapping matters for safety. The client keeps a batch whose first send failed
//! with [`TransportError::Refused`] or [`TransportError::Connect`] as never sent, and may
//! sign it again under a new ID. A browser reports a DNS, connection, TLS or CORS failure,
//! and a connection that broke after the request left, as the same opaque `TypeError`, so
//! nothing tells whether any byte left. This transport therefore maps a rejected `fetch` to
//! [`TransportError::Other`], never to `Connect`, and returns `Refused` only from its own
//! checks, made before it calls `fetch`.

use std::future::{poll_fn, Future};
use std::net::IpAddr;
use std::pin::pin;
use std::task::Poll;

use js_sys::{Array, Promise, Uint8Array};
use paykit_sdk::Timer;
use pubky_switch_client::transport::{
    BoxFuture, Header, Request, Response, TimeoutClass, Transport, TransportError,
};
use pubky_switch_core::http::{header, response_body_cap};
use pubky_switch_core::net::{check_url, UrlUse};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AbortController, AbortSignal, Headers, ReadableStreamDefaultReader, ReadableStreamReadResult,
    ReferrerPolicy, RequestCache, RequestCredentials, RequestInit, RequestMode, RequestRedirect,
    ResponseType, Url,
};

use crate::send::run_local;
use crate::timer::JsTimer;

// The global `fetch`, not `window.fetch`: a dedicated worker has no `window`, and `fetch` is a
// global in pages and workers alike.
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_name = fetch)]
    fn fetch_request(request: &web_sys::Request) -> Result<Promise, JsValue>;
}

/// The settings of a [`FetchTransport`], with the same defaults as the Switch client's native
/// HTTPS transport (`HttpConfig`).
///
/// Each call has a timeout for the whole of it, from the `fetch` call to the last byte of the
/// answer, by the class the client picked for it ([`TimeoutClass`], Switch wire 7.1 "Answer
/// sizes"):
///
/// - **[`TimeoutClass::Ordinary`]:** [`FetchConfig::request_timeout_ms`], and a share of what
///   the long timeout adds to it, in proportion to the request body: about the short timeout
///   for a body of a few KiB, the long one from a mebibyte on.
/// - **[`TimeoutClass::Bulk`]:** [`FetchConfig::long_timeout_ms`], independent of the
///   outgoing size.
/// - **[`TimeoutClass::LongPoll`]:** the wait the request names, then
///   [`FetchConfig::long_timeout_ms`] for the page.
///
/// There is no connection timeout: `fetch` has no way to set one, so connecting counts
/// against the call's timeout.
///
/// The struct is `#[non_exhaustive]`: start from [`FetchConfig::default`] and set fields.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FetchConfig {
    /// The timeout of a small call, in milliseconds: 10 s by default.
    pub request_timeout_ms: u64,
    /// The timeout of a bulk transfer, and what the feed's long poll has after its wait, in
    /// milliseconds: 40 s by default.
    pub long_timeout_ms: u64,
    /// The ports other than 443 a switch origin may use, as the client's operator allows
    /// (Switch wire 7.1). Empty by default.
    pub extra_ports: Vec<u16>,
    /// The largest answer body read, in bytes: 32 MiB by default. A larger one fails with
    /// [`TransportError::TooLarge`]. A smaller bound for a call, such as the account
    /// discovery limit, still applies.
    pub max_response_bytes: u64,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            request_timeout_ms: 10_000,
            long_timeout_ms: 40_000,
            extra_ports: Vec::new(),
            max_response_bytes: 32 << 20,
        }
    }
}

/// The Switch client's [`Transport`] on the browser's `fetch`, for `wasm32-unknown-unknown`.
///
/// It works in a page and in a dedicated worker. Each call is one `fetch` with `mode: "cors"`,
/// `credentials: "omit"`, `cache: "no-store"`, `redirect: "manual"` and
/// `referrerPolicy: "no-referrer"`. The switch origin must answer CORS requests from the page's
/// origin and expose `Switch-Service-Signature`, `Switch-Server-Time`, `Switch-Head` and
/// `Retry-After` with `Access-Control-Expose-Headers`: `fetch` hides any other header, and the
/// client fails an answer whose signature it cannot read.
///
/// What it does of the transport's duties (Switch wire 7.1):
///
/// - Before it calls `fetch`, it checks the origin with the Switch's own check: `https` only,
///   no user info, path, query or fragment, port 443 or one of
///   [`FetchConfig::extra_ports`], and an address literal only if it is public. It also
///   checks that the browser's URL parser reads the same host and port. A failed check is
///   [`TransportError::Refused`], and nothing is sent.
/// - [`FetchTransport::new`] also refuses the hosts `localhost`, `localdomain` and
///   `localdomain6` and every name under them, such as `localhost.localdomain`, in any case,
///   as [`TransportError::Refused`]: they resolve to the local machine. Both constructors
///   refuse the same way a host name of one label, such as `router`, and the names `local`,
///   `internal`, `home.arpa`, `lan`, `home`, `corp`, `private` and `intranet` and every name
///   under them: browsers and the networks they run on resolve them to local network
///   addresses.
/// - It sends the request's method, headers and body bytes exactly, and adds no header of its
///   own; `fetch` adds some, such as `Origin` and `User-Agent`, which the request signature
///   does not cover. A path or query the browser's URL parser would change, a fragment, a
///   method or header `fetch` refuses, drops, merges or trims, and a body on a `GET` or
///   `HEAD` are [`TransportError::InvalidRequest`], and nothing is sent.
/// - It follows no redirect: a redirect answer is [`TransportError::Other`]. The native
///   transport returns a redirect as an answer with its 3xx status; `fetch` hides that status,
///   with the redirect's headers and body, so this transport has no answer to return.
/// - It returns every answer, whatever its status, with the headers `fetch` exposes, names in
///   lower case. A response header whose value is not text is left out.
/// - It honours [`Request::timeout`] (see [`FetchConfig`]): when the timeout passes, it aborts
///   the `fetch` and returns [`TransportError::Timeout`]. Dropping the returned future also
///   aborts the `fetch`.
/// - It reads at most [`FetchConfig::max_response_bytes`] of an answer body, or the smaller
///   bound the Switch sets for the call, counted as the body stream hands the bytes over: a
///   body that grows past the limit fails as soon as it does, with
///   [`TransportError::TooLarge`] and an aborted `fetch`. The announced `content-length` is
///   not checked against the limit: a browser decodes a compressed answer before it hands
///   the body over, so that length can count other bytes than those read.
/// - A rejected `fetch`, or an answer body that breaks off, is [`TransportError::Other`]: a
///   browser does not say whether the request left (see the [module docs](self)). It never
///   returns [`TransportError::Connect`].
///
/// What it cannot do:
///
/// - **It cannot check the addresses a host name resolves to (Switch wire 8.2).** A browser
///   resolves names itself and does not show the addresses. Address literals are checked,
///   and the local machine and local network names above are refused, but a public name
///   that resolves to a private address is not refused here.
/// - **Repeated response headers arrive merged.** `fetch` joins the copies of a header into
///   one value, separated by `", "`, so a second copy cannot be told apart. The three headers
///   the response signature decides on, `Switch-Service-Signature`, `Switch-Server-Time` and
///   `Switch-Head`, never contain a comma, so one with a comma, or with a value that is not
///   printable ASCII, fails the answer as [`TransportError::UnreadableSignedHeader`], which the
///   client takes as a signature that does not verify. Any other repeated header is handed on
///   as one merged value.
///
/// It keeps the default [`Transport::send_before`], which returns
/// [`TransportError::FirstSendUnavailable`]. On `wasm32-unknown-unknown` a batch with a
/// first-send deadline may never get that answer: before it calls `send_before`, the Switch
/// client builds its first-send gate, which reads `std::time::Instant::now()`, and that panics
/// in a browser, so the WebAssembly instance traps. The crate's README describes this hazard,
/// and the one SDK path that submits such a batch.
///
/// Error text is fixed: it never quotes a header value, a body or a JavaScript error.
#[derive(Clone, Debug)]
pub struct FetchTransport {
    config: FetchConfig,
    allows_localhost: bool,
}

impl FetchTransport {
    /// A transport for switch origins read from `identity.json` documents (Switch wire 7.1):
    /// every origin is checked before anything is sent, `localhost`, `localdomain`,
    /// `localdomain6`, the names under them and local network names are refused, and
    /// redirects are not followed.
    pub fn new(config: FetchConfig) -> Self {
        Self {
            config,
            allows_localhost: false,
        }
    }

    /// A transport that also allows the hosts `localhost`, `localdomain` and `localdomain6`
    /// and the names under them, such as `localhost.localdomain`, for test runs against a
    /// switch on the local machine only. It still requires `https`, still
    /// runs the Switch's origin check, which refuses a private address literal such as
    /// `127.0.0.1`, still allows only port 443 and [`FetchConfig::extra_ports`], and still
    /// refuses the other local network names [`FetchTransport::new`] refuses.
    ///
    /// Use it only in a test rig on the local machine, and never ship it in an app: with it, a
    /// counterparty's `identity.json` could point the transport at a service on the user's
    /// machine (Switch wire 8.2).
    pub fn allowing_localhost_for_loopback_tests(config: FetchConfig) -> Self {
        Self {
            config,
            allows_localhost: true,
        }
    }

    /// One call, raced against its timeout. When the timeout wins, the call's future is
    /// dropped, which aborts its `fetch`.
    async fn call(&self, request: Request) -> Result<Response, TransportError> {
        let mut timeout = JsTimer.sleep(call_timeout_ms(
            &request,
            self.config.request_timeout_ms,
            self.config.long_timeout_ms,
        ));
        let mut fetch = pin!(self.fetch(request));
        poll_fn(|cx| {
            if let Poll::Ready(result) = fetch.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            timeout
                .as_mut()
                .poll(cx)
                .map(|()| Err(TransportError::Timeout))
        })
        .await
    }

    async fn fetch(&self, request: Request) -> Result<Response, TransportError> {
        let url = self.checked_url(&request)?;
        let controller = AbortController::new()
            .map_err(|_| TransportError::Other("the browser has no AbortController".into()))?;
        let _abort = AbortOnDrop(controller.clone());
        let built = build_request(&request, &url, &controller.signal())?;
        // Everything above is checked before any byte can leave; from here on, a failure has
        // an unknown outcome.
        let unknown = || TransportError::Other("the fetch failed; the outcome is unknown".into());
        let promise = fetch_request(&built).map_err(|_| unknown())?;
        let response: web_sys::Response = JsFuture::from(promise)
            .await
            .map_err(|_| unknown())?
            .unchecked_into();
        // With `redirect: "manual"`, a redirect answer comes back as an opaque response with
        // status 0, which is never followed (Switch wire 7.1). The native transport returns a
        // redirect as an answer with its 3xx status; here `fetch` hides that status, and its
        // headers and body, so there is no answer to return, only this error.
        if response.type_() == ResponseType::Opaqueredirect {
            return Err(TransportError::Other(
                "a redirect answer, which is not followed (Switch wire 7.1)".into(),
            ));
        }
        let status = response.status();
        let headers = response_headers(status, &response.headers())?;
        let limit = response_body_cap(&request.method, &request.path_and_query, status)
            .and_then(|cap| u64::try_from(cap).ok())
            .map_or(self.config.max_response_bytes, |cap| {
                self.config.max_response_bytes.min(cap)
            });
        // The limit counts the decoded bytes as they arrive, never the announced
        // `content-length`: a browser asks for compressed answers and decodes them itself, so
        // that length can be the compressed one, and a cross-origin answer hides the
        // `content-encoding` that would say so.
        let body = read_body(&response, limit).await?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }

    /// The URL a request goes to, checked (Switch wire 7.1, 8.2): the origin passes the
    /// Switch's check, is not a local network name and, for [`FetchTransport::new`], is not
    /// a name of the local machine such as `localhost`; the browser's URL
    /// parser reads the same scheme, host and port, with no user info; and it reads the path
    /// and query as exactly the signed bytes.
    fn checked_url(&self, request: &Request) -> Result<String, TransportError> {
        let refused = |reason: &str| TransportError::Refused(format!("Switch wire 7.1: {reason}"));
        if !request.path_and_query.starts_with('/') {
            return Err(TransportError::InvalidRequest(
                "the path doesn't start with '/'".into(),
            ));
        }
        let checked = check_url(
            &request.origin,
            &UrlUse::SwitchOrigin {
                extra_ports: self.config.extra_ports.clone(),
            },
        )
        .map_err(|error| TransportError::Refused(error.to_string()))?;
        // `check_url` returns a domain name in lower case, and refuses a trailing dot, so
        // `localhost.` and `router.` never get here.
        let is_name = checked.host.parse::<IpAddr>().is_err();
        if is_name && is_localhost(&checked.host) {
            if !self.allows_localhost {
                return Err(TransportError::Refused(
                    "Switch wire 8.2: the transport can't check the addresses of localhost".into(),
                ));
            }
        } else if is_name && is_local_network_name(&checked.host) {
            return Err(TransportError::Refused(
                "Switch wire 8.2: the transport can't check the addresses of a local network name"
                    .into(),
            ));
        }
        let url = Url::new(&request.url()).map_err(|_| refused("the URL doesn't parse"))?;
        if url.protocol() != "https:" || !url.username().is_empty() || !url.password().is_empty() {
            return Err(refused("the URL isn't https without user info"));
        }
        let port = match url.port().as_str() {
            "" => Some(443),
            text => text.parse::<u16>().ok(),
        };
        if port != Some(checked.port) {
            return Err(refused("the URL's port reads differently"));
        }
        let hostname = url.hostname();
        let host = hostname
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(&hostname);
        let same_host = match (host.parse::<IpAddr>(), checked.host.parse::<IpAddr>()) {
            (Ok(parsed), Ok(checked)) => parsed == checked,
            (Err(_), Err(_)) => host == checked.host,
            _ => false,
        };
        if !same_host {
            return Err(refused("the URL's host reads differently"));
        }
        // A fragment is never sent, so a path and query with one isn't what goes out.
        let href = url.href();
        if request.path_and_query.contains('#')
            || href.strip_prefix(url.origin().as_str()) != Some(request.path_and_query.as_str())
        {
            return Err(TransportError::InvalidRequest(
                "the URL changes the signed path or query".into(),
            ));
        }
        Ok(href)
    }
}

impl Transport for FetchTransport {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        let transport = self.clone();
        Box::pin(async move {
            run_local(async move { transport.call(request).await })
                .await
                .unwrap_or_else(|_| {
                    Err(TransportError::Other(
                        "the fetch task ended before it finished".into(),
                    ))
                })
        })
    }
}

/// The names under which hosts resolve to the local machine: `localhost`, and `localdomain`
/// and `localdomain6`, whose `localhost.localdomain` and `localhost6.localdomain6` many
/// systems map to a loopback address in their hosts file.
const LOCALHOST_SUFFIXES: [&str; 3] = ["localhost", "localdomain", "localdomain6"];

/// The names under which browsers, and the networks they run on, resolve hosts to local
/// network addresses: mDNS (`local`), the special-use and customary private suffixes, and the
/// private suffixes RFC 6762 Appendix G lists.
const LOCAL_NETWORK_SUFFIXES: [&str; 8] = [
    "local",
    "internal",
    "home.arpa",
    "lan",
    "home",
    "corp",
    "private",
    "intranet",
];

/// Whether `host`, a domain name in lower case, is `suffix` or a name under it.
fn is_or_is_under(host: &str, suffix: &str) -> bool {
    host.strip_suffix(suffix)
        .is_some_and(|rest| rest.is_empty() || rest.ends_with('.'))
}

/// Whether `host`, a domain name in lower case, is one of [`LOCALHOST_SUFFIXES`] or a name
/// under it, which resolve to the local machine.
fn is_localhost(host: &str) -> bool {
    LOCALHOST_SUFFIXES
        .iter()
        .any(|suffix| is_or_is_under(host, suffix))
}

/// Whether `host`, a domain name in lower case, is a name browsers resolve on the local
/// network: one label, such as `router`, or one of [`LOCAL_NETWORK_SUFFIXES`] or a name under
/// it.
fn is_local_network_name(host: &str) -> bool {
    !host.contains('.')
        || LOCAL_NETWORK_SUFFIXES
            .iter()
            .any(|suffix| is_or_is_under(host, suffix))
}

/// The body size from which an ordinary call gets the long timeout.
const LONG_BODY_BYTES: usize = 1 << 20;

/// The timeout of `request` by its class ([`FetchConfig`]), in milliseconds, as the native
/// transport computes it: for an ordinary call `short` and the share of `long - short` its
/// body is of [`LONG_BODY_BYTES`], at most `long`; for a long poll its wait, then `long`; for
/// a bulk call `long`.
fn call_timeout_ms(request: &Request, short: u64, long: u64) -> u64 {
    match request.timeout {
        TimeoutClass::Ordinary => {
            let extra = u128::from(long.saturating_sub(short)) * request.body.len() as u128
                / LONG_BODY_BYTES as u128;
            short
                .saturating_add(u64::try_from(extra).unwrap_or(u64::MAX))
                .min(long.max(short))
        }
        TimeoutClass::LongPoll { wait_s } => {
            long.saturating_add(u64::from(wait_s).saturating_mul(1_000))
        }
        // `Bulk`, and any class the client adds later: the longest timeout without a wait.
        _ => long,
    }
}

/// Aborts its `fetch` when dropped: on a timeout, an answer over the limit, an error, or a
/// caller that drops the call. Aborting a `fetch` whose body was read to the end does
/// nothing.
struct AbortOnDrop(AbortController);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Whether `value` is printable ASCII, or a tab.
fn is_text(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
}

/// The browser request for `request`, refused as [`TransportError::InvalidRequest`] when
/// `fetch` would not send it as given: a method or header it refuses, or one it drops,
/// merges, trims or upper-cases on the way.
fn build_request(
    request: &Request,
    url: &str,
    signal: &AbortSignal,
) -> Result<web_sys::Request, TransportError> {
    let invalid = |what: &str| TransportError::InvalidRequest(what.into());
    let headers = Headers::new().map_err(|_| invalid("the headers"))?;
    for Header { name, value } in &request.headers {
        // The rule of the native transport: a value is printable ASCII or tabs. `fetch` would
        // send other characters up to U+00FF as single bytes.
        if !is_text(value) {
            return Err(invalid("a header value"));
        }
        headers
            .append(name, value)
            .map_err(|_| invalid("a header"))?;
    }
    let init = RequestInit::new();
    init.set_method(&request.method);
    init.set_headers(&headers);
    init.set_mode(RequestMode::Cors);
    init.set_credentials(RequestCredentials::Omit);
    init.set_cache(RequestCache::NoStore);
    init.set_redirect(RequestRedirect::Manual);
    init.set_referrer_policy(ReferrerPolicy::NoReferrer);
    init.set_signal(Some(signal));
    if !request.body.is_empty() {
        init.set_body(&Uint8Array::from(request.body.as_slice()));
    }
    let built = web_sys::Request::new_with_str_and_init(url, &init)
        .map_err(|_| invalid("the method, a header or the body"))?;
    if built.method() != request.method {
        return Err(invalid("the method"));
    }
    // A request's headers drop the names `fetch` forbids without an error, trim spaces and
    // tabs around a value, and join repeated names with ", ": each header must read back
    // exactly as given.
    let sent = built.headers();
    for Header { name, value } in &request.headers {
        if sent.get(name).ok().flatten().as_deref() != Some(value.as_str()) {
            return Err(invalid("a header fetch doesn't send as given"));
        }
    }
    Ok(built)
}

/// The answer's headers that `fetch` exposes, names in lower case (Switch wire 7.1). A header
/// the response signature decides on, `Switch-Service-Signature`, `Switch-Server-Time` or
/// `Switch-Head`, is handed on exactly or fails the answer as
/// [`TransportError::UnreadableSignedHeader`]: one with a value that is not printable ASCII,
/// or with a comma, which is how `fetch` joins repeated copies and which none of them
/// contains. Any other header whose value isn't text is left out: nothing reads it.
fn response_headers(status: u16, map: &Headers) -> Result<Vec<Header>, TransportError> {
    let signed = [header::SERVICE_SIGNATURE, header::SERVER_TIME, header::HEAD];
    let unreadable = TransportError::UnreadableSignedHeader { status };
    let mut headers = Vec::new();
    for entry in map.entries() {
        // Each entry is a `[name, value]` pair of strings.
        let entry: Array = entry.map_err(|_| unreadable.clone())?.unchecked_into();
        let (Some(mut name), Some(value)) = (entry.get(0).as_string(), entry.get(1).as_string())
        else {
            return Err(unreadable);
        };
        name.make_ascii_lowercase();
        if signed.contains(&name.as_str()) {
            if value.contains([',', '\t']) || !is_text(&value) {
                return Err(unreadable);
            }
        } else if !is_text(&value) {
            continue;
        }
        headers.push(Header { name, value });
    }
    Ok(headers)
}

/// The answer body, read chunk by chunk, at most `limit` bytes: one more fails with
/// [`TransportError::TooLarge`] before it is copied. The buffer grows with the chunks read,
/// never by the `content-length` the switch announces, which may not match the body.
async fn read_body(response: &web_sys::Response, limit: u64) -> Result<Vec<u8>, TransportError> {
    let broken = || TransportError::Other("the answer broke off; the outcome is unknown".into());
    // No body stream: the answer has no body, as for a `HEAD` request or a 204.
    let Some(stream) = response.body() else {
        return Ok(Vec::new());
    };
    let reader: ReadableStreamDefaultReader = stream.get_reader().unchecked_into();
    let mut body = Vec::new();
    loop {
        let read: ReadableStreamReadResult = JsFuture::from(reader.read())
            .await
            .map_err(|_| broken())?
            .unchecked_into();
        if read.get_done() == Some(true) {
            return Ok(body);
        }
        let chunk: Uint8Array = read.get_value().dyn_into().map_err(|_| broken())?;
        let start = body.len();
        let total = start.saturating_add(chunk.length() as usize);
        if u64::try_from(total).unwrap_or(u64::MAX) > limit {
            return Err(TransportError::TooLarge(limit));
        }
        body.resize(total, 0);
        chunk.copy_to(&mut body[start..]);
    }
}
