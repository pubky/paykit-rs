//! The Paykit SDK's payment flows on the durable store: the payer's wallet keeps its state in
//! an [`OpfsStore`], closes it while a payment is unfinished, opens it again from disk and
//! resumes.
//!
//! The flows run in the world of `tests/pay_flow/` (`tests/world/`): two Paykit Apps on the
//! Switch client's in-memory scripted switches, one WebAssembly instance, no network. One test
//! resumes a payment left in flight, as `tests/pay_flow/resume.rs` does on a `MemoryStore`;
//! the other stops the wallet after each commit of a payment with the Switch client's
//! `FaultStore`, as paykit-sdk's native flow test `crash.rs` does. Each test stores under a
//! name of its own and removes its store at the end.
//!
//! The target runs in a dedicated worker only, with a browser profile on disk
//! (`rig/runner.mjs`, "Target modes").
//!
//! [`OpfsStore`]: paykit_browser::store::OpfsStore

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

use wasm_bindgen_test::wasm_bindgen_test_configure;

wasm_bindgen_test_configure!(run_in_dedicated_worker);

mod crash;
#[allow(dead_code)]
#[path = "../opfs/mod.rs"]
mod opfs;
mod resume;
mod support;
// The world is shared with the flow targets, and this one leaves some of its helpers unused,
// such as the wallet on a `MemoryStore`.
#[allow(dead_code)]
#[path = "../world/mod.rs"]
mod world;
