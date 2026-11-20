//! A server that never answers: the call ends at the deadline of its timeout class, with
//! `TransportError::Timeout`, and the browser closes the connection.

use pubky_switch_client::transport::{TimeoutClass, Transport, TransportError};
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::support::assert_closed_by_browser;
use crate::switch_rig::wait::{now_ms, race};
use crate::switch_rig::{fresh_tag, request, transport_with};

/// The short timeout these tests configure, in milliseconds: the deadline of an ordinary call
/// with an empty body.
const SHORT_MS: u64 = 300;

/// The long timeout these tests configure, in milliseconds: the deadline of a bulk call, and
/// what a long poll gets after its wait.
const LONG_MS: u64 = 600;

/// How much later than its deadline a call may end, in milliseconds: room for timers that
/// fire late in a busy browser, far below the rig's 60 s cap on a hanging answer.
const LATE_MS: u64 = 1_500;

/// How long the rig may take to see the connection closed after the call ended, in
/// milliseconds.
const CLOSE_BOUND_MS: u64 = 5_000;

/// For each case `(name, class, deadline_ms)`, sends a `GET` to `/rig/hang` with a fresh tag
/// and `extra` in its query, in timeout class `class`, through a loopback transport with
/// [`SHORT_MS`] and [`LONG_MS`] as its timeouts, and checks that it ends with `Timeout` no
/// earlier than `deadline_ms` and at most [`LATE_MS`] later, and that the rig then sees its
/// connection closed by the browser within [`CLOSE_BOUND_MS`], with nothing left open.
async fn assert_times_out_and_closes(extra: &str, cases: &[(&str, TimeoutClass, u64)]) {
    let transport = transport_with(|config| {
        config.request_timeout_ms = SHORT_MS;
        config.long_timeout_ms = LONG_MS;
    });
    for &(name, class, deadline_ms) in cases {
        let tag = fresh_tag(name);
        let mut hang = request("GET", &format!("/rig/hang?tag={tag}{extra}"));
        hang.timeout = class;
        let started = now_ms();
        let mut call = transport.send(hang);
        // Bounded: a call that never ends fails here instead of hanging the run.
        let outcome = race(&mut call, deadline_ms + LATE_MS + 2_000).await;
        let elapsed = now_ms() - started;
        drop(call);
        let error = match outcome {
            None => panic!("{name}: no outcome {elapsed} ms after the call started"),
            Some(Ok(response)) => panic!("{name}: an answer, status {}", response.status),
            Some(Err(error)) => error,
        };
        assert_eq!(error, TransportError::Timeout, "{name}");
        // `JsTimer` never ends a sleep before its delay on `performance.now()`; the wall clock
        // may read a millisecond less.
        let earliest = deadline_ms as f64 - 2.0;
        let latest = (deadline_ms + LATE_MS) as f64;
        assert!(
            (earliest..=latest).contains(&elapsed),
            "{name}: ended after {elapsed} ms, for a deadline of {deadline_ms} ms"
        );

        let closed_after = assert_closed_by_browser(name, &tag, CLOSE_BOUND_MS).await;
        console_log!(
            "[deadline] {name}: deadline {deadline_ms} ms, ended after {elapsed:.0} ms, \
             rig saw the close within {closed_after:.0} ms"
        );
    }
}

/// A server that reads the request and never answers: the call ends at its class's deadline
/// with `Timeout`, and the browser closes the connection. The class picks the deadline: the
/// short timeout for an ordinary call with an empty body, the long one for a bulk call, and
/// the wait plus the long one for a long poll.
#[wasm_bindgen_test]
async fn test_deadline_unanswered_call_ends_at_its_class_deadline() {
    let cases = [
        ("unanswered-ordinary", TimeoutClass::Ordinary, SHORT_MS),
        ("unanswered-bulk", TimeoutClass::Bulk, LONG_MS),
        (
            "unanswered-long-poll",
            TimeoutClass::LongPoll { wait_s: 1 },
            1_000 + LONG_MS,
        ),
    ];
    assert_times_out_and_closes("", &cases).await;
}

/// A server that sends the head of its answer and then no body: the deadline covers the whole
/// call, up to the last byte of the body, so the call ends at it with `Timeout`, and the
/// browser closes the connection.
#[wasm_bindgen_test]
async fn test_deadline_stalled_body_ends_at_the_deadline() {
    let cases = [
        ("stalled-body-ordinary", TimeoutClass::Ordinary, SHORT_MS),
        ("stalled-body-bulk", TimeoutClass::Bulk, LONG_MS),
    ];
    assert_times_out_and_closes("&stage=headers", &cases).await;
}
