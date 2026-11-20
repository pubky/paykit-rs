//! What CORS preflights a call costs. Every request of the Switch client carries Switch
//! headers, which are not CORS-safelisted, so the browser sends an `OPTIONS` preflight before
//! it unless it holds a fresh one for the URL. The rig counts them.
//!
//! How long an engine keeps a preflight, and by what it matches one, differ between engines,
//! so most counts are printed for the record rather than asserted. What is asserted holds in
//! every engine: each call reaches the rig once, a URL never used before is preflighted, and an
//! answer with `access-control-max-age: 0` is never reused, also on a path the rig forwards to
//! the service. The rig answers every preflight itself, by its design, so none reaches the
//! service.

use pubky_switch_client::transport::Transport;
use pubky_switch_core::http::header;
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::support::{default_mode_get, preflighted, SWITCH_TIMESTAMP};
use crate::switch_rig::{disarm_fault, fresh_tag, path_hits, tag_count, transport, TagCount};

/// Sends a `GET` with a Switch header to each rig path in turn, checks each answer is a 200,
/// and returns what the rig counted for `tag`, which every path carries in its query. Prints
/// the count, for the record of each engine and context, and checks that each call reached
/// the rig exactly once.
async fn rig_calls(scenario: &str, tag: &str, paths: &[String]) -> TagCount {
    let transport = transport();
    for path in paths {
        let response = transport
            .send(preflighted("GET", path))
            .await
            .unwrap_or_else(|error| panic!("{path}: {error:?}"));
        assert_eq!(response.status, 200, "{path}");
    }
    let count = tag_count(tag).await;
    let calls = paths.len() as u64;
    console_log!(
        "[preflight] {scenario}: {calls} calls, {} preflights",
        count.preflights
    );
    assert_eq!(count.calls(), calls, "{scenario}: {count:?}");
    count
}

/// The same URL twice, with the rig's default `access-control-max-age: 600`: one preflight,
/// or two when the engine does not reuse it; the first call is always preflighted.
///
/// The same two requests through the global `fetch` with the browser's default cache mode,
/// instead of the transport's `no-store`, are counted and printed too, not asserted: an engine
/// may skip its preflight cache for a request that skips the HTTP cache.
#[wasm_bindgen_test]
async fn test_preflight_same_url_twice() {
    let tag = fresh_tag("preflight-same-url");
    let path = format!("/rig/echo?tag={tag}");
    let count = rig_calls("same URL twice, max-age 600", &tag, &[path.clone(), path]).await;
    assert!((1..=2).contains(&count.preflights), "{count:?}");

    let tag = fresh_tag("preflight-default-mode");
    let path = format!("/rig/echo?tag={tag}");
    let headers = [(header::TIMESTAMP, SWITCH_TIMESTAMP)];
    let mut answered = 0;
    for _ in 0..2 {
        if default_mode_get(&path, &headers).await.is_some() {
            answered += 1;
        }
    }
    let count = tag_count(&tag).await;
    console_log!(
        "[preflight] same URL twice, max-age 600, default cache mode instead of no-store: \
         {answered} of 2 answered, {} calls, {} preflights",
        count.calls(),
        count.preflights
    );
}

/// Three paths, each called once, with `access-control-max-age: 600`. Each URL is new, so
/// none can match a preflight held from before: the browser preflights each one.
#[wasm_bindgen_test]
async fn test_preflight_each_new_path() {
    let tag = fresh_tag("preflight-paths");
    let paths = [
        format!("/rig/echo?tag={tag}"),
        format!("/rig/status?code=200&tag={tag}"),
        format!("/rig/bytes?n=1&seed=0&tag={tag}"),
    ];
    let count = rig_calls("three paths, max-age 600", &tag, &paths).await;
    assert_eq!(count.preflights, 3, "{count:?}");
}

/// The same path with a query that changes on each call, as the feed's `after=` does, with
/// `access-control-max-age: 600`. Each URL is new, so the browser preflights each call.
#[wasm_bindgen_test]
async fn test_preflight_same_path_changing_query() {
    let tag = fresh_tag("preflight-query");
    let paths: Vec<String> = (1..=3)
        .map(|after| format!("/rig/echo?tag={tag}&after={after}"))
        .collect();
    let count = rig_calls("same path, changing query, max-age 600", &tag, &paths).await;
    assert_eq!(count.preflights, 3, "{count:?}");
}

/// The same URL twice, back to back, with no `access-control-max-age`: an engine may keep the
/// preflight for a few seconds by default, or not at all.
#[wasm_bindgen_test]
async fn test_preflight_same_url_without_max_age() {
    let tag = fresh_tag("preflight-no-max-age");
    let path = format!("/rig/echo?tag={tag}&max_age=none");
    let count = rig_calls("same URL twice, no max-age", &tag, &[path.clone(), path]).await;
    assert!((1..=2).contains(&count.preflights), "{count:?}");
}

/// The same URL three times with `access-control-max-age: 0`: a preflight answer that may not
/// be kept is never reused, so every call is preceded by a preflight.
#[wasm_bindgen_test]
async fn test_preflight_max_age_zero_precedes_every_call() {
    let tag = fresh_tag("preflight-max-age-0");
    let path = format!("/rig/echo?tag={tag}&max_age=0");
    let count = rig_calls(
        "same URL three times, max-age 0",
        &tag,
        &[path.clone(), path.clone(), path],
    )
    .await;
    assert_eq!(count.preflights, 3, "{count:?}");
}

/// Sends a `GET` with a Switch header to `path` once for each query in `queries`, on a path
/// the rig forwards to the service, and checks that each call reached the service once,
/// whatever the service answers, and that the rig answered at least one preflight for the
/// path. Returns the preflights the rig answered for the path.
async fn forwarded_calls(scenario: &str, path: &str, queries: &[&str]) -> u64 {
    let transport = transport();
    for query in queries {
        transport
            .send(preflighted("GET", &format!("{path}{query}")))
            .await
            .unwrap_or_else(|error| panic!("{scenario}: {error:?}"));
    }
    let calls = queries.len() as u64;
    let get = path_hits(path, Some("GET")).await;
    console_log!(
        "[preflight] forwarded, {scenario}: {calls} calls, {} preflights",
        get.preflights
    );
    assert_eq!(get.forwarded, calls, "{scenario}: {get:?}");
    assert!(get.preflights >= 1, "{scenario}: {get:?}");
    get.preflights
}

/// Paths the rig forwards to the service, for each `access-control-max-age` the rig can
/// answer, which each call asks for in its query (`max_age`): the same URL twice, and the same
/// path with a changing `after=` query. Each call reaches the service once, and with
/// `access-control-max-age: 0` every call is preceded by a preflight. The paths are made up:
/// what the service answers them, and that the query reaches it, does not matter here. A rig
/// fault left armed would make the rig close every connection after its answer, so it is
/// disarmed first.
#[wasm_bindgen_test]
async fn test_preflight_to_a_forwarded_path_is_answered_by_the_rig() {
    disarm_fault().await;
    for max_age in ["0", "600", "none"] {
        let tag = fresh_tag("preflight-forwarded");
        let same = format!("/v1/preflight-probe/{tag}/same");
        let query = format!("?max_age={max_age}");
        let same_preflights = forwarded_calls(
            &format!("same URL twice, max-age {max_age}"),
            &same,
            &[&query, &query],
        )
        .await;
        let feed = format!("/v1/preflight-probe/{tag}/feed");
        let pages: Vec<String> = (1..=3)
            .map(|after| format!("?max_age={max_age}&after={after}"))
            .collect();
        let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
        let feed_preflights = forwarded_calls(
            &format!("same path, changing query, max-age {max_age}"),
            &feed,
            &pages,
        )
        .await;
        if max_age == "0" {
            assert_eq!((same_preflights, feed_preflights), (2, 3));
        }
    }
}
