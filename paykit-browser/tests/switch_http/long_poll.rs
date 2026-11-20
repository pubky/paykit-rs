//! An app's wait on its feed, a long poll the service holds until the account changes or the
//! wait passes (Switch wire 7.4.4), through `FetchTransport`.

use std::sync::Arc;

use paykit_browser::clock::JsClock;
use paykit_browser::fetch::FetchTransport;
use paykit_sdk::MessageDelivery;
use pubky_switch_client::app::{FeedWait, Woken, MAX_WAIT_SECONDS};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::transport::{
    BoxFuture, Request, Response, TimeoutClass, Transport, TransportError,
};
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::switch_rig::wait::{both, now_ms, race, sleep};
use crate::switch_rig::{request, transport, transport_with};
use crate::switch_world::{settle, SwitchWorld, PAYEE, PAYER};
use crate::world::{draft, DAY_MS};

/// The wait the app asks for when a message is on its way, in seconds: long enough that an
/// early return cannot be the wait passing.
const EARLY_WAIT_S: u32 = 10;

/// How long after the wait starts the payee sends its message, in milliseconds.
const SEND_AFTER_MS: u64 = 1_000;

/// The wait of the poll on a quiet feed, on a transport whose margin is shorter than it, in
/// seconds.
const MARGIN_WAIT_S: u32 = 4;

/// The margin that transport adds to a long poll's wait (`FetchConfig::long_timeout_ms`), in
/// milliseconds: shorter than [`MARGIN_WAIT_S`].
const SHORT_MARGIN_MS: u64 = 2_000;

/// How much earlier than its wait a quiet poll may be measured to end, in milliseconds: the
/// service starts its wait after the request arrives, but a browser may coarsen its clock.
const CLOCK_SLACK_MS: f64 = 100.0;

/// How much later than its wait a quiet poll may end, in milliseconds, for the request and
/// the answer on a loaded machine.
const LATE_MS: f64 = 3_000.0;

/// A transport that hands every call to a `FetchTransport` with the default settings, except
/// long polls, which it sends on `short`, one whose `long_timeout_ms` is [`SHORT_MARGIN_MS`]:
/// so only the measured poll has the short margin, and the join and the settling read, whose
/// bulk calls `long_timeout_ms` bounds too, keep the default deadlines.
struct ShortMargin {
    fetch: FetchTransport,
    short: FetchTransport,
}

impl Transport for ShortMargin {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        if matches!(request.timeout, TimeoutClass::LongPoll { .. }) {
            return self.short.send(request);
        }
        self.fetch.send(request)
    }
}

/// Waits on `wait` for at most its wait and [`LATE_MS`] more: what it woke to, and how long it
/// took in milliseconds.
///
/// # Panics
///
/// When it is still waiting then.
async fn timed_wait(wait: FeedWait) -> (Woken, f64) {
    let bound = u64::from(wait.seconds()) * 1_000 + LATE_MS as u64;
    let started = now_ms();
    let woken = race(&mut Box::pin(wait.wait()), bound)
        .await
        .expect("the wait ends within its bound");
    (woken, now_ms() - started)
}

/// The payer's wallet waits on its feed for [`EARLY_WAIT_S`]; a second later the payee's shop
/// sends it a Payment Request. The wait wakes with something to read after the request was
/// sent and long before its wait passes, and the wallet's next receive applies that request.
#[wasm_bindgen_test]
async fn test_long_poll_returns_early_when_a_message_arrives() {
    let w = SwitchWorld::enrol(&[PAYER, PAYEE], &[(PAYER, "wallet"), (PAYEE, "shop")]).await;
    let mut wallet = w.join("payer:wallet").await;
    let mut shop = w.join("payee:shop").await;
    // The wallet invites the payee, so the payer's switch admits the shop's request (Paykit
    // spec 1), and settles, so the wait starts quiet.
    wallet.app_mut().invite(&w.id(PAYEE)).await.unwrap();
    settle(&mut wallet).await;

    let wait = wallet.app().feed_wait(EARLY_WAIT_S);
    assert_eq!(wait.seconds(), EARLY_WAIT_S);
    let started = now_ms();
    let waiting = async {
        let woken = wait.wait().await;
        (woken, now_ms())
    };
    let sending = async {
        sleep(SEND_AFTER_MS).await;
        let sent_at = now_ms();
        let sent = shop
            .send_request(w.id(PAYER), draft("1", JsClock.now_ms() + DAY_MS))
            .await;
        (sent, sent_at)
    };
    let bound = u64::from(EARLY_WAIT_S) * 1_000 + LATE_MS as u64;
    let (waited, sent) = both(waiting, sending, bound).await;
    let (woken, woke_at) = waited.expect("the wait ends within its bound");
    let (sent, sent_at) = sent.expect("the request is sent within the bound");
    let sent = sent.unwrap();
    assert!(
        matches!(sent.delivery, MessageDelivery::Accepted { .. }),
        "{:?}",
        sent.delivery
    );

    let took = woke_at - started;
    console_log!(
        "[long-poll] wait {EARLY_WAIT_S} s, message sent at {:.0} ms: woke {woken:?} at {took:.0} \
         ms, {:.0} ms after the send started",
        sent_at - started,
        woke_at - sent_at
    );
    assert_eq!(woken, Woken::Ready);
    assert!(
        woke_at >= sent_at,
        "the wait woke {:.0} ms before the request was sent",
        sent_at - woke_at
    );
    assert!(
        took < f64::from(EARLY_WAIT_S) * 1_000.0 / 2.0,
        "the wait woke only after {took:.0} ms"
    );

    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(summary.applied, [sent.msg_id], "{summary:?}");
}

/// The transport's deadline for a long poll is the wait the request names plus
/// `long_timeout_ms`, so it never cuts a poll the service holds for its wait.
///
/// The client asks for at most the service's maximum wait, 25 s (Switch wire 7.4.4): a wait
/// asked for longer is cut to it before anything is sent. So every poll's deadline is longer
/// than the longest the service holds one, 65 s for 25 s with the default settings, and no
/// poll needs to run the full 25 s to show it. Here the margin is shorter than the wait: a
/// wallet whose transport sends its long polls with a 2 s `long_timeout_ms` ([`ShortMargin`])
/// waits 4 s on a feed nothing arrives on, and the service holds the poll for the whole wait
/// and then answers an empty page, which the wait reports as quiet: the poll is not cut. A
/// bulk call on the transport of the long polls, which has no wait, ends at the 2 s margin,
/// run beside the poll so the margin costs no extra time.
#[wasm_bindgen_test]
async fn test_long_poll_deadline_outlasts_the_wait() {
    let w = SwitchWorld::enrol(&[PAYER], &[(PAYER, "wallet")]).await;
    let short = transport_with(|config| config.long_timeout_ms = SHORT_MARGIN_MS);
    let margin = ShortMargin {
        fetch: transport(),
        short: short.clone(),
    };
    let mut wallet = w.join_with("payer:wallet", Arc::new(margin)).await;
    settle(&mut wallet).await;

    assert_eq!(MAX_WAIT_SECONDS, 25);
    assert_eq!(wallet.app().feed_wait(60).seconds(), MAX_WAIT_SECONDS);
    assert_eq!(
        wallet.app().feed_wait(MAX_WAIT_SECONDS).seconds(),
        MAX_WAIT_SECONDS
    );

    let wait = wallet.app().feed_wait(MARGIN_WAIT_S);
    assert_eq!(wait.seconds(), MARGIN_WAIT_S);
    let mut slow = request("GET", &format!("/rig/slow?ms={}", MARGIN_WAIT_S * 1_000));
    slow.timeout = TimeoutClass::Bulk;
    let bulk = async {
        let started = now_ms();
        let outcome = short.send(slow).await;
        (outcome.map(|response| response.status), now_ms() - started)
    };
    let bound = u64::from(MARGIN_WAIT_S) * 1_000 + LATE_MS as u64;
    let (waited, bulk) = both(timed_wait(wait), bulk, bound).await;
    let (woken, took) = waited.expect("the wait ends within its bound");
    let (bulk, bulk_took) = bulk.expect("the bulk call ends within the bound");
    console_log!(
        "[long-poll] margin {SHORT_MARGIN_MS} ms, wait {MARGIN_WAIT_S} s: {woken:?} after \
         {took:.0} ms; a bulk call beside it: {bulk:?} after {bulk_took:.0} ms"
    );
    assert_eq!(bulk, Err(TransportError::Timeout), "the margin is not 2 s");
    assert_eq!(woken, Woken::Quiet, "the poll was cut");
    let asked_ms = f64::from(MARGIN_WAIT_S) * 1_000.0;
    assert!(
        took >= asked_ms - CLOCK_SLACK_MS,
        "the poll ended after {took:.0} ms"
    );
    assert!(
        took < asked_ms + LATE_MS,
        "the poll ended after {took:.0} ms"
    );
}
