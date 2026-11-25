//! Futures built on the heap from a frame of their own.
//!
//! An `async fn`'s poll runs on the host's thread, and a debug build gives every future it
//! builds a stack slot of the future's full size in that poll's frame, `Box::pin(self.step())`
//! included: the future is built in the slot before it moves to the heap, and the slot stays
//! for the whole poll, under every deeper call. The paths that end in the Dispatch client's
//! send are the deepest a wallet runs, and the client's own frames there are the larger share;
//! Paykit's slots on those paths add up beside them until a host's thread overflows.

use std::future::Future;
use std::pin::Pin;

/// Build `make`'s future on the heap.
///
/// The future's bytes take this function's frame, which has returned before the future is
/// first polled, so the caller's poll frame keeps only the box. The future is the one
/// `Box::pin` would hold: it runs, and is dropped or cancelled with its caller, exactly as an
/// inline `.await` of it would be. Paykit's larger steps use this rather than `Box::pin` at
/// the call site; `inline(never)` keeps the frame apart in any build.
#[inline(never)]
pub(crate) fn on_heap<F: Future>(make: impl FnOnce() -> F) -> Pin<Box<F>> {
    Box::pin(make())
}
