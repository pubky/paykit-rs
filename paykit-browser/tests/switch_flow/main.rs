//! The Paykit SDK's one-time Payment Request flow against a real Switch service, in a browser.
//!
//! `rig/test.sh --switch` runs the service behind the rig on `https://localhost:<rig port>`
//! (`rig/switch-rig.mjs`). Each test asks the rig for a world of its own (`POST /rig/world`):
//! a payer and a payee, enrolled on the service with the real signer, and an app of each,
//! approved for `paykit_pay` and `paykit_request`. The test joins both apps to that one
//! service through `FetchTransport`, on the browser's clock (`JsClock`) and timers
//! (`JsTimer`), with in-memory state and the SDK's `FakeRail` (`switch_world.rs`). The
//! request draft and the reader of the payer's stored request are the scripted flows' own
//! (`tests/world/`).
//!
//! The payer's wallet invites the payee, the payee's shop sends a one-time Payment Request, the
//! wallet reads it open as it was sent and pays it through the rail, the shop receives the
//! Payment Proof and confirms it with a Payment Receipt, and the wallet stores the receipt.
//! The other tests pin down a payment the rail refuses, a request the payer's switch does not
//! admit until the payer invites the payee, and a request to an identity that never enrolled.
//! The steps and the outcomes they check follow paykit-sdk's native flow tests
//! (`paykit-sdk/tests/flows/one_time.rs`), with the service's answers in place of a scripted
//! switch's. The service has no test clock, so nothing here moves time: expiries and periods
//! are left to the scripted flows (`tests/request_flow/`, `tests/pay_flow/`).
//!
//! The transport exists only on `wasm32-unknown-unknown`, so natively the target is empty.
//! wasm-bindgen-test runs it in a browser page or in a dedicated worker; the runner's
//! environment picks the browser and the context (`rig/runner.mjs`).

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

mod delivery;
mod one_time;
#[allow(dead_code)]
#[path = "../switch_rig/mod.rs"]
mod switch_rig;
mod switch_world;
// Only `DAY_MS`, `draft` and `stored_request` are used here; the scripted world goes unused.
#[allow(dead_code)]
#[path = "../world/mod.rs"]
mod world;

use paykit_sdk::Paykit;

use switch_world::SwitchWorld;
use world::{draft, stored_request, DAY_MS};

/// The payer's identity.
pub const PAYER: &str = "payer";
/// The payee's identity.
pub const PAYEE: &str = "payee";

/// A new world on the service, with the payer's wallet and the payee's shop joined. The
/// wallet has not invited the payee yet.
pub async fn world() -> (SwitchWorld, Paykit, Paykit) {
    let w = SwitchWorld::enrol(&[PAYER, PAYEE], &[(PAYER, "wallet"), (PAYEE, "shop")]).await;
    let wallet = w.join("payer:wallet").await;
    let shop = w.join("payee:shop").await;
    (w, wallet, shop)
}

/// [`world()`], with the wallet having invited the payee, a conversation the payer starts, as
/// at a checkout, so the payer's switch admits the payee's Payment Requests (Paykit spec 1).
pub async fn invited_world() -> (SwitchWorld, Paykit, Paykit) {
    let (w, mut wallet, shop) = world().await;
    wallet.app_mut().invite(&w.id(PAYEE)).await.unwrap();
    (w, wallet, shop)
}
