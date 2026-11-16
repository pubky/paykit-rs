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

// Task spawning and timers live in `paykit_lib::rt` (tokio on native,
// spawn_local + setTimeout on wasm32).
