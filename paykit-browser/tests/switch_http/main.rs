//! How the real Switch service's long poll and caching behave over the browser's `fetch`.
//!
//! `rig/test.sh --switch` runs the service behind the rig on `https://localhost:<rig port>`
//! (`rig/switch-rig.mjs`), which speaks HTTP/1.1. Each test asks the rig for a world of its
//! own (`POST /rig/world`) and joins its apps to the service through `FetchTransport`
//! (`tests/switch_flow/switch_world.rs`).
//!
//! - `long_poll.rs`: an app's wait on its feed (Switch wire 7.4.4) returns as soon as a
//!   counterparty's message arrives, and returns empty when its wait passes with nothing to
//!   show. The client asks the service for at most its 25 s maximum, and the transport's
//!   deadline for a long poll is the wait plus a margin, so even a margin shorter than the
//!   wait never cuts a healthy poll.
//! - `connection_limit.rs`: how many long polls a page or a worker can hold open on one
//!   HTTP/1.1 origin before an ordinary call to it waits for a connection. The limit is the
//!   browser's, so it is measured and printed; what is asserted holds in every engine: beside
//!   four held polls, an ordinary call completes, and a call that waited for a connection
//!   completes once a held poll ends or is dropped.
//! - `dropped_poll.rs`: held polls the browser gives up while the service still holds them,
//!   dropped by the app or ended by the transport's deadline, leave the rig and the service
//!   answering, also after the service's wait has passed.
//! - `identity_read.rs`: the service's public identity read answers with
//!   `cache-control: public, max-age=60, must-revalidate`, and every read through the client
//!   still reaches the service, since the transport asks for `cache: "no-store"`.
//!
//! The rig counts the requests per forwarded path (`/rig/hits`) for the whole run, across
//! engines and contexts, so every test reads the counts of paths of its own world. The
//! numbers that differ between engines are printed with `console_log!`, so they land in the
//! run log.
//!
//! wasm-bindgen-test runs the tests of a target one at a time. A panic aborts the WebAssembly
//! instance, so every wait here has a bound and a test fails rather than hangs.
//!
//! The transport exists only on `wasm32-unknown-unknown`, so natively the target is empty.
//! wasm-bindgen-test runs it in a browser page or in a dedicated worker; the runner's
//! environment picks the browser and the context (`rig/runner.mjs`).

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

mod connection_limit;
mod dropped_poll;
mod identity_read;
mod long_poll;
#[allow(dead_code)]
#[path = "../switch_rig/mod.rs"]
mod switch_rig;
#[path = "../switch_flow/switch_world.rs"]
mod switch_world;
// Only `DAY_MS` and `draft` are used here; the scripted world goes unused.
#[allow(dead_code)]
#[path = "../world/mod.rs"]
mod world;
