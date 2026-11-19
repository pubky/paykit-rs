//! Running browser futures where the host traits need `Send` ones: [`run_local`].
//!
//! The host traits of the SDK and the Switch client return `Send` futures: for example
//! `paykit_sdk::Timer::sleep`, `Transport::send` and `HomeserverReader::get` of
//! `pubky-switch-client`, and `paykit_sdk::Rail::pay`. `Send` means "this value may be moved
//! to another thread". A future that waits on the browser by awaiting a JavaScript promise
//! through `JsFuture` is not `Send`: `JsFuture` keeps its state in an `Rc`, a reference count
//! that is not thread safe. A `JsValue` alone does not make a future non-`Send`: on targets
//! without threads, as this crate's are, wasm-bindgen marks `JsValue` as `Send` and `Sync`.
//! Still, a JavaScript value belongs to the thread (the page or the worker) that made it, and
//! is meaningless on any other.
//!
//! [`run_local`] joins the two. It starts the browser future on this thread's event loop with
//! `wasm_bindgen_futures::spawn_local`, and gives back a different future that only waits for
//! the browser future's output on a one-shot channel. That returned future holds nothing but
//! the receiving end of the channel, so it is `Send` whenever the output is.
//!
//! # Why this is sound
//!
//! There is no `unsafe` here, and no wrapper that claims a value is `Send` when it is not: the
//! compiler checks every bound. The browser future and every JavaScript value it holds stay
//! inside the task that `spawn_local` created, on the thread that called [`run_local`]; they
//! never cross a thread boundary. Only the channel's receiving end crosses the `Send`
//! boundary, and the output that travels through it must itself be `Send`.
//!
//! On single-threaded `wasm32` there is in any case one thread: the returned future is polled
//! on the same thread that runs the browser future.
//!
//! # Why the crate refuses WebAssembly threads
//!
//! With WebAssembly threads (the `atomics` target feature) each thread is a separate worker
//! with its own event loop and its own JavaScript values. The bridge would still be memory
//! safe, but the returned future could then be polled on one worker while the browser future
//! runs on another, and the host pieces built on it would mix up the two event loops: for
//! example, `performance.now()` counts from a different start in each worker. This crate is
//! built and tested only single-threaded, and `src/lib.rs` refuses a threaded build at compile
//! time.

use std::fmt;
use std::future::{poll_fn, Future};
use std::pin::pin;
use std::task::Poll;

use futures_channel::oneshot;

/// The local future that [`run_local`] started was dropped before it finished, so it has no
/// output to give.
///
/// `wasm_bindgen_futures` drops an unfinished task only when polling it unwinds. A
/// `wasm32-unknown-unknown` build aborts on a panic by default, so there an unwinding poll
/// ends the whole WebAssembly instance first; this error keeps a caller from waiting forever
/// in a build that unwinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTaskDropped;

impl fmt::Display for LocalTaskDropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the local task was dropped before it finished")
    }
}

impl std::error::Error for LocalTaskDropped {}

/// Start `future` on this thread's event loop, and return a `Send` future that resolves
/// with its output.
///
/// `future` may hold JavaScript values and await JavaScript promises: it never leaves this
/// thread (see the [module docs](self)). It is handed to `wasm_bindgen_futures::spawn_local`
/// at once, which first polls it in this thread's next microtask, after `run_local` returns,
/// and it runs whether or not the returned future is polled.
///
/// - The returned future resolves with `Ok(output)` once `future` finishes.
/// - Dropping the returned future cancels `future`: the next time this thread's microtask
///   queue runs, `future` is dropped without being polled again, so its destructors run then,
///   even if it was waiting on something that never comes. A `future` that never started is
///   dropped without being polled at all.
/// - If `future` is dropped before it finishes for any other reason, which only a build that
///   unwinds on a panic allows (see [`LocalTaskDropped`]), the returned future is written to
///   resolve with [`LocalTaskDropped`] instead of waiting forever. The tests here run builds
///   that abort on a panic, so they do not reach this case.
///
/// The output must be `Send`, because it is handed to the returned future, which may be
/// moved to another thread. A `JsValue` output passes that bound here, since wasm-bindgen
/// marks `JsValue` as `Send` on targets without threads (see the [module docs](self)).
///
/// Call it on `wasm32` from code that runs in a browser page or worker, where the event loop
/// drives `spawn_local`'s tasks.
pub fn run_local<F>(
    future: F,
) -> impl Future<Output = Result<F::Output, LocalTaskDropped>> + Send + 'static
where
    F: Future + 'static,
    F::Output: Send + 'static,
{
    let (mut sender, receiver) = oneshot::channel();
    wasm_bindgen_futures::spawn_local(async move {
        let mut future = pin!(future);
        let output = poll_fn(|cx| {
            // The receiver is gone: stop without polling `future` again. Returning drops it
            // with the rest of this task. `poll_canceled` also registers this task to be
            // woken when the receiver is dropped later, so a `future` that waits forever is
            // still stopped.
            if sender.poll_canceled(cx).is_ready() {
                return Poll::Ready(None);
            }
            future.as_mut().poll(cx).map(Some)
        })
        .await;
        if let Some(output) = output {
            // An error means the receiver was dropped after the last poll; nobody wants the
            // output, which is dropped here.
            let _ = sender.send(output);
        }
    });
    async move { receiver.await.map_err(|oneshot::Canceled| LocalTaskDropped) }
}
