//! The store outside a dedicated worker: in a page, [`OpfsStore::open`] refuses with
//! [`OpenError::NotDedicatedWorker`] and creates nothing.
//!
//! The target runs in a page only, with a browser profile on disk (`rig/runner.mjs`, "Target
//! modes"), where every engine lets a page read the Origin Private File System.

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

use paykit_browser::store::{OpenError, OpfsStore, WrappingKey};
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

wasm_bindgen_test_configure!(run_in_browser);

#[allow(dead_code)]
#[path = "../opfs/mod.rs"]
mod opfs;

/// The name of the store the test tries to open.
const STORE_NAME: &str = "page";

/// A page cannot open the store: the call fails with `NotDedicatedWorker`, and no directory
/// of that name appears in the origin's OPFS root.
#[wasm_bindgen_test]
async fn test_store_page_open_is_refused() {
    let result = OpfsStore::open(STORE_NAME, WrappingKey::Origin).await;
    assert!(
        matches!(result, Err(OpenError::NotDedicatedWorker)),
        "{result:?}"
    );

    // The runner gives this target a browser profile on disk, where every engine gives a page
    // the OPFS root, so a refused root is a broken rig and fails the test.
    let root = opfs::root().await;
    assert!(
        !opfs::exists(&root, STORE_NAME).await,
        "the refused open created a store directory"
    );
}
