//! The rig's `hold` fault on a call the Switch client makes to the real service: the public
//! read of an identity's `identity.json` (Switch wire 7.4.15), a `GET` that changes nothing.
//! The rig forwards a held request only once its hold has passed, so the read takes at least
//! the hold; a read whose deadline comes first ends with `TransportError::Timeout`, and the
//! browser closes its connection.

use std::sync::Arc;

use paykit_browser::clock::JsClock;
use paykit_browser::fetch::FetchTransport;
use pubky_switch_client::transport::TransportError;
use pubky_switch_client::{ClientError, Credentials, SwitchClient};
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::faults::{with_fault, Fault, FaultKind};
use crate::switch_rig::wait::{now_ms, wait_until};
use crate::switch_rig::{account_path, path_hits, transport, transport_with};
use crate::switch_world::{SwitchWorld, PAYER};

/// The hold of a read that then completes, in milliseconds.
const SHORT_HOLD_MS: u32 = 400;

/// The deadline of a read on the short transport, in milliseconds: its `long_timeout_ms`,
/// since the client sends a public read as a bulk call.
const DEADLINE_MS: u64 = 1_000;

/// The hold of a read that its deadline ends first, in milliseconds: far past
/// [`DEADLINE_MS`] and [`LATE_MS`], so the read cannot end with the hold.
const LONG_HOLD_MS: u32 = 5_000;

/// How much earlier than the hold, or the deadline, a call may be measured to end, in
/// milliseconds: a browser may coarsen its clock.
const CLOCK_SLACK_MS: f64 = 100.0;

/// How much later than the hold, or the deadline, a call may end, in milliseconds, for the
/// request and the answer on a loaded machine.
const LATE_MS: f64 = 2_000.0;

/// How long the rig may take to see a closed connection, in milliseconds.
const RIG_BOUND_MS: u64 = 3_000;

/// A new world with the payer enrolled, the path of the payer's public identity read, and a
/// Switch client for the payer's account on `fetch`.
async fn payer_reader(fetch: FetchTransport) -> (SwitchWorld, String, SwitchClient) {
    let w = SwitchWorld::enrol(&[PAYER], &[]).await;
    let identity = w.identity(PAYER);
    let doc = w.identity_doc(PAYER);
    let inbox = doc.inbox();
    let client = SwitchClient::new(
        &inbox.url,
        identity.public(),
        Credentials::Signer {
            identity: identity.public(),
            key: Arc::new(identity.key().clone()),
        },
        Some(inbox.service_key),
        Arc::new(fetch),
        Arc::new(JsClock),
    )
    .expect("the client is made");
    let path = account_path(&identity.public(), "identity");
    (w, path, client)
}

/// A `hold` fault of `ms` on the next `GET` of `path`.
fn hold(path: &str, ms: u32) -> Fault {
    Fault {
        method: "GET",
        path: path.to_owned(),
        kind: FaultKind::Hold { ms },
        after: 0,
    }
}

/// The rig holds the payer's public identity read for [`SHORT_HOLD_MS`] and then forwards it:
/// the read returns the document the payer's signer published, no earlier than the hold, and
/// the rig forwarded it once.
#[wasm_bindgen_test]
async fn test_switch_faults_hold_delays_a_read_by_its_hold() {
    let (w, path, client) = payer_reader(transport()).await;
    let before = path_hits(&path, Some("GET")).await;
    let timed = async {
        let started = now_ms();
        let read = client.public_identity().await;
        (read, now_ms() - started)
    };
    let (read, took) = with_fault(&hold(&path, SHORT_HOLD_MS), timed).await;
    let hits = path_hits(&path, Some("GET")).await.since(before);
    console_log!("[hold] a read held {SHORT_HOLD_MS} ms took {took:.0} ms; rig counts {hits:?}");
    let public = read.unwrap_or_else(|error| panic!("the held read: {error:?}"));
    assert_eq!(public.identity.hash(), w.identity_doc(PAYER).hash());
    let hold_ms = f64::from(SHORT_HOLD_MS);
    assert!(
        took >= hold_ms - CLOCK_SLACK_MS,
        "the read took only {took:.0} ms"
    );
    assert!(took < hold_ms + LATE_MS, "the read took {took:.0} ms");
    assert_eq!((hits.forwarded, hits.refused), (1, 0), "{hits:?}");
}

/// The rig holds the payer's public identity read for [`LONG_HOLD_MS`], through a transport
/// whose deadline for it is [`DEADLINE_MS`]: the read ends at that deadline with
/// `TransportError::Timeout`, long before the hold would end, and the rig sees the browser
/// close the connection, with nothing left open. The hold fault fired on the read, so the
/// read reached the rig, and the rig never sent it on to the service.
#[wasm_bindgen_test]
async fn test_switch_faults_hold_past_the_deadline_times_out_and_closes() {
    let short = transport_with(|config| config.long_timeout_ms = DEADLINE_MS);
    let (_w, path, client) = payer_reader(short).await;
    let before = path_hits(&path, Some("GET")).await;
    let timed = async {
        let started = now_ms();
        let read = client.public_identity().await;
        (read.err(), now_ms() - started)
    };
    let (error, took) = with_fault(&hold(&path, LONG_HOLD_MS), timed).await;
    let closed = wait_until(
        RIG_BOUND_MS,
        || path_hits(&path, Some("GET")),
        |hits| hits.open == 0,
    )
    .await
    .since(before);
    console_log!(
        "[hold] a read held {LONG_HOLD_MS} ms with a {DEADLINE_MS} ms deadline: {error:?} after \
         {took:.0} ms; rig counts then {closed:?}"
    );
    assert!(
        matches!(error, Some(ClientError::Transport(TransportError::Timeout))),
        "{error:?}"
    );
    let deadline_ms = DEADLINE_MS as f64;
    assert!(
        took >= deadline_ms - CLOCK_SLACK_MS,
        "the read ended after only {took:.0} ms"
    );
    assert!(
        took < deadline_ms + LATE_MS,
        "the read ended after {took:.0} ms"
    );
    assert_eq!(
        (closed.forwarded, closed.refused, closed.open),
        (0, 0, 0),
        "the rig sent the read on, or still holds it: {closed:?}"
    );
}
