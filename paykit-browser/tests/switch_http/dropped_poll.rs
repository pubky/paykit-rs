//! Held feed polls the browser gives up while the service still holds them: dropped by the
//! app, or past the transport's deadline. The rig forwards each poll to the service, so the
//! browser closes a connection the rig is still waiting on the service for; the rig and the
//! service must go on answering, also after the service's wait has passed.

use std::sync::Arc;

use paykit_browser::fetch::FetchTransport;
use pubky_switch_client::app::Woken;
use pubky_switch_client::transport::TransportError;
use pubky_switch_client::transport::{BoxFuture, Request, Response, TimeoutClass, Transport};
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::switch_rig::wait::{now_ms, race, sleep, wait_until};
use crate::switch_rig::{
    account_path, disarm_fault, path_hits, request, transport, transport_with,
};
use crate::switch_world::{settle, SwitchWorld, PAYER};

/// The polls dropped at once.
const DROPPED: usize = 3;

/// The wait each dropped poll asks for, in seconds: short, since each test waits for the
/// service's wait to pass before it checks the rig and the service again.
const DROPPED_WAIT_S: u32 = 2;

/// How long a new poll is driven before it is dropped, in milliseconds: enough for the
/// client to sign it and hand it to `fetch`.
const START_MS: u64 = 100;

/// How long the rig may take to see polls arrive or close, in milliseconds: the bound of each
/// wait on its counts.
const RIG_BOUND_MS: u64 = 3_000;

/// How long after the service's wait the test checks again, in milliseconds: by then the
/// service has answered, or given up, every poll the browser closed.
const AFTER_WAIT_MS: f64 = 1_000.0;

/// The deadline of a long poll on [`PollDeadline`], in milliseconds: shorter than
/// [`DROPPED_WAIT_S`].
const CUT_MS: u64 = 500;

/// A transport that hands every call to a `FetchTransport` with the default settings,
/// except long polls, which it sends with the ordinary class on one whose ordinary timeout
/// is [`CUT_MS`]: so a poll ends at that deadline while the service still holds it. The
/// timeout class is the transport's own and is not sent, so the request goes out as the
/// client signed it.
struct PollDeadline {
    fetch: FetchTransport,
    cut: FetchTransport,
}

impl PollDeadline {
    fn new() -> Self {
        Self {
            fetch: transport(),
            cut: transport_with(|config| config.request_timeout_ms = CUT_MS),
        }
    }
}

impl Transport for PollDeadline {
    fn send(&self, mut request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        if matches!(request.timeout, TimeoutClass::LongPoll { .. }) {
            request.timeout = TimeoutClass::Ordinary;
            return self.cut.send(request);
        }
        self.fetch.send(request)
    }
}

/// Once the service's wait of polls started at `started` has passed, checks that the rig
/// still answers a route of its own and forwards an app's ordinary call to the service: the
/// rig's count of `feed`, and a read of the app's feed with no wait.
///
/// # Panics
///
/// When a call fails.
async fn assert_rig_and_service_answer(
    wallet: &mut paykit_sdk::Paykit,
    feed: &str,
    started: f64,
    case: &str,
) {
    let after = started + f64::from(DROPPED_WAIT_S) * 1_000.0 + AFTER_WAIT_MS;
    sleep((after - now_ms()).max(0.0) as u64).await;
    let health = transport()
        .send(request("GET", "/rig/health"))
        .await
        .unwrap_or_else(|error| panic!("{case}: /rig/health: {error:?}"));
    assert_eq!(health.status, 200, "{case}: /rig/health");
    let before = path_hits(feed, None).await;
    wallet
        .receive(None)
        .await
        .unwrap_or_else(|error| panic!("{case}: a read of the feed: {error:?}"));
    let after = path_hits(feed, None).await;
    assert!(
        after.forwarded > before.forwarded,
        "{case}: the read was not forwarded: {before:?} then {after:?}"
    );
    assert_eq!(after.open, 0, "{case}: {after:?}");
}

/// The wallet holds [`DROPPED`] polls of its feed at once, each waiting [`DROPPED_WAIT_S`],
/// and drops them all while the service holds them, before any answer head arrives. The rig
/// sees every connection close; once the service's wait has passed, the rig still answers
/// and still forwards the wallet's next read of its feed, which the service answers. A rig
/// fault left armed would make the rig close each connection after its answer, so it is
/// disarmed first.
#[wasm_bindgen_test]
async fn test_dropped_poll_held_polls_dropped_leave_the_rig_and_the_service_answering() {
    disarm_fault().await;
    let w = SwitchWorld::enrol(&[PAYER], &[(PAYER, "wallet")]).await;
    let mut wallet = w.join("payer:wallet").await;
    settle(&mut wallet).await;
    let feed = account_path(&w.id(PAYER), "feed");

    let started = now_ms();
    let mut polls = Vec::new();
    for held in 1..=DROPPED {
        let mut poll = Box::pin(wallet.app().feed_wait(DROPPED_WAIT_S).wait());
        if let Some(woken) = race(&mut poll, START_MS).await {
            panic!("poll {held} ended at once: {woken:?}");
        }
        polls.push(poll);
    }
    let held = wait_until(
        RIG_BOUND_MS,
        || path_hits(&feed, None),
        |hits| hits.open == DROPPED as u64,
    )
    .await;
    assert_eq!(held.open, DROPPED as u64, "the polls held: {held:?}");
    drop(polls);
    let closed = wait_until(
        RIG_BOUND_MS,
        || path_hits(&feed, None),
        |hits| hits.open == 0,
    )
    .await;
    console_log!(
        "[dropped-poll] {DROPPED} held polls dropped after {:.0} ms: rig counts {held:?} then \
         {closed:?}",
        now_ms() - started
    );
    assert_eq!(
        closed.open, 0,
        "the rig still holds a dropped poll: {closed:?}"
    );

    assert_rig_and_service_answer(&mut wallet, &feed, started, "dropped").await;
}

/// The wallet's poll, waiting [`DROPPED_WAIT_S`], goes through a transport whose deadline for
/// it is [`CUT_MS`]: the poll ends at that deadline with `ReadNow`, as a wait whose read
/// failed, while the service still holds it, and the rig sees its connection close. Once the
/// service's wait has passed, the rig still answers and still forwards the wallet's next read
/// of its feed, which the service answers. A rig fault left armed is disarmed first, as
/// above.
#[wasm_bindgen_test]
async fn test_dropped_poll_past_its_deadline_leaves_the_rig_and_the_service_answering() {
    disarm_fault().await;
    let w = SwitchWorld::enrol(&[PAYER], &[(PAYER, "wallet")]).await;
    let mut wallet = w
        .join_with("payer:wallet", Arc::new(PollDeadline::new()))
        .await;
    settle(&mut wallet).await;
    let feed = account_path(&w.id(PAYER), "feed");

    let wait = wallet.app().feed_wait(DROPPED_WAIT_S);
    assert_eq!(wait.seconds(), DROPPED_WAIT_S);
    let started = now_ms();
    let bound = u64::from(DROPPED_WAIT_S) * 1_000 + RIG_BOUND_MS;
    let woken = race(&mut Box::pin(wait.wait()), bound)
        .await
        .expect("the wait ends within its bound");
    let took = now_ms() - started;
    let closed = wait_until(
        RIG_BOUND_MS,
        || path_hits(&feed, None),
        |hits| hits.open == 0,
    )
    .await;
    console_log!(
        "[dropped-poll] a poll with a {CUT_MS} ms deadline and a {DROPPED_WAIT_S} s wait: \
         {woken:?} after {took:.0} ms; rig counts then {closed:?}"
    );
    assert_eq!(woken, Woken::ReadNow, "the poll was not cut");
    assert!(
        took < f64::from(DROPPED_WAIT_S) * 1_000.0,
        "the poll ended only after {took:.0} ms"
    );
    assert_eq!(
        closed.open, 0,
        "the rig still holds the cut poll: {closed:?}"
    );

    assert_rig_and_service_answer(&mut wallet, &feed, started, "past its deadline").await;
}
