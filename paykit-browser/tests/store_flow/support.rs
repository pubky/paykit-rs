//! Opening an [`OpfsStore`] for the flows, and checking that it is held open.

use std::sync::Arc;

use paykit_browser::store::{OpenError, OpfsStore, WrappingKey};
use pubky_switch_client::store::StateStore;

/// Open the store `name`, for the SDK.
pub async fn open_store(name: &str) -> Arc<dyn StateStore> {
    Arc::new(
        OpfsStore::open(name, WrappingKey::Origin)
            .await
            .expect("open the store"),
    )
}

/// Check that the store `name` is held open: a second open is refused while the first store
/// lives. So a later open that succeeds proves the first store closed its files.
pub async fn assert_held_open(name: &str) {
    let refused = OpfsStore::open(name, WrappingKey::Origin)
        .await
        .expect_err("a second open while the store is open");
    assert_eq!(refused, OpenError::InUse);
}
