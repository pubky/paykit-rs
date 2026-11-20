//! How many long polls a page or a worker can hold open on one HTTP/1.1 origin before an
//! ordinary call to that origin waits for a connection.
//!
//! Over HTTP/1.1 each request in progress takes a connection of its own, and a browser opens
//! only so many to one origin; a held long poll keeps its connection for its whole wait. The
//! limit is the engine's, so it is measured and printed, not asserted. What is asserted holds
//! in every engine: beside [`ASSERTED_HELD`] held polls an ordinary call completes, and a
//! call that waited for a connection completes once a held poll ends or is dropped.

use std::future::Future;
use std::pin::Pin;

use pubky_switch_client::app::Woken;
use pubky_switch_client::transport::{BoxFuture, Response, Transport, TransportError};
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::switch_rig::wait::{now_ms, race, wait_until};
use crate::switch_rig::{account_path, disarm_fault, hits_request, read_hits, transport};
use crate::switch_world::{settle, SwitchWorld, PAYER};

/// The wait each held poll asks for, in seconds: longer than it takes to open up to
/// [`MAX_HELD`] polls and probe beside each until a call stalls for [`STALL_MS`], about 1.7 s
/// on a local machine, with room for a loaded one. A poll that returned during the probing
/// would end it early.
const HELD_WAIT_S: u32 = 5;

/// The most polls the test holds at once. An engine that still completes an ordinary call
/// beside this many is reported as having no limit up to it.
const MAX_HELD: usize = 10;

/// The held polls beside which an ordinary call must complete, in every engine: two fewer
/// than the six connections per origin that browsers commonly open over HTTP/1.1.
const ASSERTED_HELD: usize = 4;

/// How long an ordinary call to the rig may take before it counts as waiting for a
/// connection, in milliseconds. A call the rig answers itself takes a few milliseconds.
const STALL_MS: u64 = 1_000;

/// How long the call that waited may take to complete once a held poll is dropped, in
/// milliseconds: the drop aborts the poll's `fetch`, which frees its connection at once.
const FREED_MS: u64 = 2_000;

/// How long a new poll is driven before the test probes beside it, in milliseconds: enough
/// for the client to sign it and hand it to `fetch`.
const START_MS: u64 = 100;

/// How long the rig may take to see a new poll arrive, in milliseconds.
const ARRIVE_MS: u64 = 3_000;

/// A held poll: the future of one feed wait.
type HeldPoll = Pin<Box<dyn Future<Output = Woken>>>;

/// What an ordinary call beside the held polls did.
enum Probe<'a> {
    /// It completed in [`STALL_MS`].
    Completed {
        /// How long it took, in milliseconds.
        ms: f64,
        /// The requests to the feed the rig showed in progress.
        open: u64,
    },
    /// It did not complete in [`STALL_MS`]: the call, still waiting.
    Stalled(BoxFuture<'a, Result<Response, TransportError>>),
}

/// Sends ordinary calls to the rig through `transport`, one at a time, asking it how many
/// requests to `feed` are in progress, until it shows `held` or a call does not complete in
/// [`STALL_MS`].
///
/// # Panics
///
/// When a call fails, or the rig does not show `held` polls within [`ARRIVE_MS`].
async fn probe_beside<'a>(transport: &'a impl Transport, feed: &str, held: usize) -> Probe<'a> {
    let probe = wait_until(
        ARRIVE_MS,
        move || async move {
            let started = now_ms();
            let mut call = transport.send(hits_request(feed, None));
            let Some(answer) = race(&mut call, STALL_MS).await else {
                return Probe::Stalled(call);
            };
            let ms = now_ms() - started;
            let hits = read_hits(&answer.unwrap_or_else(|error| panic!("/rig/hits: {error:?}")));
            Probe::Completed {
                ms,
                open: hits.open,
            }
        },
        |probe| match probe {
            Probe::Completed { open, .. } => *open >= held as u64,
            Probe::Stalled(_) => true,
        },
    )
    .await;
    if let Probe::Completed { open, .. } = probe {
        assert!(
            open >= held as u64,
            "the rig shows {open} of {held} polls in progress"
        );
    }
    probe
}

/// One app holds 1, 2, ... long polls of its feed at once, up to [`MAX_HELD`], each waiting
/// [`HELD_WAIT_S`]; beside each new one, an ordinary call to the same origin (the rig's count
/// of the feed's requests) must complete and show every poll in progress at the service.
/// The first number of held polls beside which the call does not complete in [`STALL_MS`] is
/// the engine's connection limit for the origin, printed. It must be above [`ASSERTED_HELD`].
/// The first held poll is then dropped, which frees its connection, and the call that waited
/// must complete within [`FREED_MS`]. A rig fault left armed would make the rig close each
/// connection after its answer, so it is disarmed first.
#[wasm_bindgen_test]
async fn test_connection_limit_ordinary_call_completes_beside_held_polls() {
    disarm_fault().await;
    let w = SwitchWorld::enrol(&[PAYER], &[(PAYER, "wallet")]).await;
    let mut wallet = w.join("payer:wallet").await;
    settle(&mut wallet).await;
    let feed = account_path(&w.id(PAYER), "feed");
    let transport = transport();

    // The polls still held when the test ends are dropped with `polls`, which aborts their
    // `fetch`; the service answers each later, to a closed connection.
    let mut polls: Vec<HeldPoll> = Vec::new();
    let mut stalled = None;
    let mut probes = Vec::new();
    for held in 1..=MAX_HELD {
        let mut poll: HeldPoll = Box::pin(wallet.app().feed_wait(HELD_WAIT_S).wait());
        if let Some(woken) = race(&mut poll, START_MS).await {
            panic!("poll {held} ended at once: {woken:?}");
        }
        polls.push(poll);
        match probe_beside(&transport, &feed, held).await {
            Probe::Completed { ms, .. } => probes.push(format!("{held}: {ms:.0} ms")),
            Probe::Stalled(call) => {
                stalled = Some((held, call));
                break;
            }
        }
    }
    console_log!(
        "[connection-limit] an ordinary call beside n held polls: {}",
        probes.join(", ")
    );

    let Some((limit, mut call)) = stalled else {
        console_log!(
            "[connection-limit] no limit up to {MAX_HELD} held polls: an ordinary call \
             completed beside each"
        );
        return;
    };
    assert!(
        limit > ASSERTED_HELD,
        "an ordinary call stalled beside only {limit} held polls"
    );
    let dropped_at = now_ms();
    drop(polls.remove(0));
    let answer = race(&mut call, FREED_MS)
        .await
        .expect("the ordinary call completes once a held poll is dropped")
        .unwrap_or_else(|error| panic!("/rig/hits: {error:?}"));
    let freed_after = now_ms() - dropped_at;
    let hits = read_hits(&answer);
    console_log!(
        "[connection-limit] limit {limit}: beside {limit} held polls an ordinary call did not \
         complete in {STALL_MS} ms; it completed {freed_after:.0} ms after the first poll was \
         dropped, with {} polls in progress at the rig",
        hits.open
    );
}
