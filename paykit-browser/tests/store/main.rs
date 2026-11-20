//! The host's durable store, `paykit_browser::store::OpfsStore`, checked against the Switch
//! client's `StateStore` contract and on real files of the Origin Private File System.
//!
//! The contract cases (`contract.rs`) are written once, over `&dyn StateStore`, and run on two
//! stores:
//!
//! - natively, on the Switch client's `MemoryStore`, so each case is first shown to hold for
//!   the client's own store (`on_memory.rs`);
//! - in a browser, on `OpfsStore`, each case on a fresh store that is then dropped and opened
//!   again, to check that the contents it left are read back from disk (`on_opfs.rs`).
//!
//! The cases cover the order of a batch's writes, `scan`'s prefix, `after`, `limit` and byte
//! order, `Delete` and `DeletePrefix`, namespaces with no keys, ownership (a plain `commit`
//! refused on an owned namespace, a wrong or stale owner applying nothing, a second owner
//! `Busy`, a lease released by its last holder) and the sizes: a 16 MiB value and a 32 MiB batch
//! accepted, `WriteBatch` refusing more, and `commit_if` counting its condition.
//!
//! In the browser only (`opening.rs`), on the store's real files: committed state survives
//! dropping and opening the store again; a store open in this worker cannot be opened again,
//! from this worker or from a nested dedicated worker, until it is dropped; a lease that
//! outlives its store keeps it open, and refuses `destroy`, until the lease drops; damage to
//! the newest file opens the store at the other file's older state, and the next commit lands
//! and survives; damage to both files is refused as corrupt and leaves both unchanged; an
//! invalid name creates nothing. Every test removes the store it created, its key record included.
//!
//! The target runs in a dedicated worker only, with a browser profile on disk
//! (`rig/runner.mjs`, "Target modes").

#![cfg(any(not(target_arch = "wasm32"), target_os = "unknown"))]

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);

mod contract;
#[cfg(not(target_arch = "wasm32"))]
mod on_memory;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod on_opfs;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod opening;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[allow(dead_code)]
#[path = "../opfs/mod.rs"]
mod opfs;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod support;
