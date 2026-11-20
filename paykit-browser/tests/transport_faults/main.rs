//! `FetchTransport` when the network misbehaves, in a browser: the rig's fault routes on
//! `https://localhost:<rig port>`, which `rig/test.sh --switch` serves with TLS and CORS
//! (`rig/switch-rig.mjs`).
//!
//! - `deadline.rs`: a server that never answers, or sends the head of its answer and then no
//!   body, ends the call at the deadline of its timeout class with `TransportError::Timeout`,
//!   and the rig sees the browser close the connection, so the abort reached the network.
//! - `dropped_call.rs`: dropping the future `send` returned, before the head arrives and while
//!   the body is awaited, closes the connection, and the same transport then completes an
//!   ordinary call.
//! - `cache.rs`: an answer the rig marks cacheable for a minute is fetched from the rig again
//!   on the next call, since the transport asks for `cache: "no-store"`.
//! - `preflight.rs`: how many CORS preflights a call with a Switch request header costs, for
//!   the same URL, other paths, a changing query, and each `access-control-max-age` the rig
//!   can answer, also for paths the rig forwards to the service.
//!
//! The rig counts the requests whose query has a `tag` (`/rig/count`) and the requests per
//! forwarded path (`/rig/hits`), and keeps those counters for the whole run, across engines
//! and contexts, so every test uses tags and paths no other request has used. The counts the
//! engines may differ on are printed with `console_log!`, not asserted.
//!
//! wasm-bindgen-test runs the tests of a target one at a time. A panic aborts the WebAssembly
//! instance, so every wait here has a bound and a test fails rather than hangs.
//!
//! The transport exists only on `wasm32-unknown-unknown`, so natively the target is empty.
//! wasm-bindgen-test runs it in a browser page or in a dedicated worker; the runner's
//! environment picks the browser and the context (`rig/runner.mjs`).

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

mod cache;
mod deadline;
mod dropped_call;
mod preflight;
mod support;
#[allow(dead_code)]
#[path = "../switch_rig/mod.rs"]
mod switch_rig;
