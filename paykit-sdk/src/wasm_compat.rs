//! THROWAWAY (#411): wasm32 `Send` relaxation.
//!
//! Browser futures from reqwest/pubky hold `Rc<RefCell<..>>` and JS closures,
//! so they are `!Send`. wasm32-unknown-unknown is single-threaded, so dropping
//! the `Send` requirement there is sound as long as nothing moves futures
//! across Web Workers. On native targets these aliases are identical to the
//! original `+ Send` types, so the native public API does not change.

use std::{future::Future, pin::Pin};

/// Boxed future that is `Send` on native targets and not required to be on wasm32.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// Boxed future that is `Send` on native targets and not required to be on wasm32.
#[cfg(target_arch = "wasm32")]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Bound that means `Send` on native targets and nothing on wasm32.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}
/// Bound that means `Send` on native targets and nothing on wasm32.
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// Boxed stream that is `Send` on native targets and not required to be on wasm32.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxStream<'a, T> = Pin<Box<dyn futures_util::Stream<Item = T> + Send + 'a>>;
/// Boxed stream that is `Send` on native targets and not required to be on wasm32.
#[cfg(target_arch = "wasm32")]
pub type BoxStream<'a, T> = Pin<Box<dyn futures_util::Stream<Item = T> + 'a>>;

/// Erase a future's type into a [`BoxFuture`] (`FutureExt::boxed` on native).
pub fn boxed<'a, F>(future: F) -> BoxFuture<'a, F::Output>
where
    F: Future + MaybeSend + 'a,
{
    Box::pin(future)
}

/// Spawn a background task.
///
/// Native: `tokio::spawn` (multi-thread capable, needs `Send`).
/// wasm32: `tokio::task::spawn_local`. RUNTIME HAZARD: this panics unless it is
/// called inside a `tokio::task::LocalSet`, and tokio's `time` driver needs a
/// tokio runtime that wasm32-unknown-unknown cannot drive by itself. The
/// runtime story (LocalSet vs. wasm_bindgen_futures::spawn_local plus a
/// different JoinHandle/AbortHandle type) is owned by a later step.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}

/// See the native variant.
#[cfg(target_arch = "wasm32")]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    tokio::task::spawn_local(future)
}
