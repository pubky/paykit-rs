//! A caller that drops the future `send` returned while the request is in flight: the
//! browser closes the connection, and the transport keeps working.

use pubky_switch_client::transport::Transport;
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::support::assert_closed_by_browser;
use crate::switch_rig::wait::{race, sleep, wait_until};
use crate::switch_rig::{fresh_tag, request, rig_bytes, tag_count, transport};

/// How long the call runs before the test checks it is in flight, in milliseconds.
const IN_FLIGHT_MS: u64 = 200;

/// How long the rig may take to see the request arrive, and then the connection close after
/// the drop, in milliseconds.
const RIG_BOUND_MS: u64 = 5_000;

/// Starts a `GET` to `/rig/hang` with a fresh tag and `extra` in its query through one
/// transport with the default timeouts, waits until the rig holds it open and `settle_ms`
/// more, drops the call, and checks that the rig sees the connection closed by the browser
/// within [`RIG_BOUND_MS`]. The same transport then fetches an answer of its own, whole.
async fn assert_drop_closes_and_transport_recovers(name: &str, extra: &str, settle_ms: u64) {
    // The default timeouts, 10 s for this call: only the drop can end it within the test.
    let transport = transport();
    let tag = fresh_tag(name);
    let mut call = transport.send(request("GET", &format!("/rig/hang?tag={tag}{extra}")));
    // Polled for a while, so the call starts; it must not finish.
    if let Some(outcome) = race(&mut call, IN_FLIGHT_MS).await {
        panic!(
            "{name}: the call ended before the drop: {:?}",
            outcome.map(|response| response.status)
        );
    }
    let arrived = wait_until(RIG_BOUND_MS, || tag_count(&tag), |count| count.open == 1).await;
    assert_eq!(
        arrived.open, 1,
        "{name}: the request is not in flight at the rig: {arrived:?}"
    );
    sleep(settle_ms).await;

    drop(call);
    let closed_after = assert_closed_by_browser(name, &tag, RIG_BOUND_MS).await;
    console_log!("[dropped] {name}: rig saw the close {closed_after:.0} ms after the drop");

    let (n, seed) = (4_096, 9);
    let response = transport
        .send(request("GET", &format!("/rig/bytes?n={n}&seed={seed}")))
        .await
        .unwrap_or_else(|error| panic!("{name}: the call after the drop: {error:?}"));
    assert_eq!(response.status, 200, "{name}: the call after the drop");
    assert!(
        response.body == rig_bytes(n, seed),
        "{name}: the call after the drop read a body of {} bytes that differs",
        response.body.len()
    );
}

/// Dropping a call before the head of its answer arrives closes the connection, and the same
/// transport then completes an ordinary call.
#[wasm_bindgen_test]
async fn test_dropped_call_before_the_head_closes_the_connection() {
    assert_drop_closes_and_transport_recovers("dropped-before-head", "", 0).await;
}

/// Dropping a call while it waits for the body, after the rig sent the head, closes the
/// connection, and the same transport then completes an ordinary call. The rig sent the head
/// as it took the request, at least 100 ms before the drop.
#[wasm_bindgen_test]
async fn test_dropped_call_during_the_body_closes_the_connection() {
    assert_drop_closes_and_transport_recovers("dropped-during-body", "&stage=headers", 100).await;
}
