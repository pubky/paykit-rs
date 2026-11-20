//! The transport asks for `cache: "no-store"`: an answer marked cacheable is fetched again on
//! the next call.

use pubky_switch_client::transport::Transport;
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::support::default_mode_get;
use crate::switch_rig::{fresh_tag, request, tag_count, transport};

/// Two calls to the same URL, whose answer says `cache-control: public, max-age=60`, both
/// reach the rig: the bodies count 1 and 2, and the rig counted two requests. Without
/// `no-store` a browser may answer the second call from its cache.
///
/// What the same two requests get through the global `fetch` with the browser's default cache
/// mode is printed, not asserted: whether an engine stores the rig's answers at all, which
/// come with a certificate the engine was told to accept, is up to the engine.
#[wasm_bindgen_test]
async fn test_cache_cacheable_answer_reaches_the_rig_on_every_call() {
    let transport = transport();
    let tag = fresh_tag("cacheable");
    let path = format!("/rig/cacheable?tag={tag}");
    for expected in ["1", "2"] {
        let response = transport.send(request("GET", &path)).await.unwrap();
        assert_eq!(response.status, 200, "{path}");
        assert_eq!(
            response.header("cache-control"),
            Some("public, max-age=60"),
            "the answer is not marked cacheable"
        );
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            expected,
            "the call did not reach the rig"
        );
    }
    assert_eq!(tag_count(&tag).await.calls(), 2);

    let control = format!("/rig/cacheable?tag={}", fresh_tag("cacheable-default"));
    let first = default_mode_get(&control, &[]).await;
    let second = default_mode_get(&control, &[]).await;
    console_log!(
        "[cache] default cache mode, same URL twice: bodies {first:?} then {second:?} \
         (\"1\" twice means the second came from the browser's cache)"
    );
}
