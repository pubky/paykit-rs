//! The bindings' own runtime. Every SDK future, host callback and network call runs on its
//! threads, never on the thread a host calls from: a host thread only waits for an answer.
use crate::WalletError;
use std::{
    cell::Cell,
    future::Future,
    sync::{Mutex, OnceLock},
    time::Duration,
};

/// The stack of every runtime thread. The deepest wallet flow needs under 0.5 MiB in a
/// release build and under 1.9 MiB in a debug one; 4 MiB leaves room for either.
pub(crate) const THREAD_STACK_BYTES: usize = 4 * 1024 * 1024;

/// How long a wallet command waits for the host's `pay` before it records the payment as
/// possibly sent. The host's call goes on to its end and its answer is recorded then.
pub(crate) const PAY_LIMIT: Duration = Duration::from_secs(30);

/// How long a command waits for any other asynchronous host call: a payment lookup, the
/// Allowance history, a proof check or a Homeserver call.
pub(crate) const HOST_CALL_LIMIT: Duration = Duration::from_secs(10);

const WORKER_THREADS: usize = 2;

fn runtime() -> Result<&'static tokio::runtime::Runtime, WalletError> {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    static STARTING: Mutex<()> = Mutex::new(());
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    // One builder at a time, so a second runtime is never built and then dropped, which
    // would block inside the caller's async context.
    let _starting = STARTING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let built = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .thread_stack_size(THREAD_STACK_BYTES)
        .thread_name("paykit-wallet")
        .enable_all()
        .build()
        .map_err(|_| WalletError::RuntimeUnavailable)?;
    Ok(RUNTIME.get_or_init(|| built))
}

/// Start `work` on the wallet runtime. It runs to its end even when nobody waits for it.
pub(crate) fn spawn<F>(work: F) -> Result<tokio::task::JoinHandle<F::Output>, WalletError>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(runtime()?.spawn(work))
}

/// Run `work` on the wallet runtime and wait for its answer. A host that stops waiting
/// only stops this wait: `work` itself runs to its end.
pub(crate) async fn run<F, T>(work: F) -> Result<T, WalletError>
where
    F: Future<Output = Result<T, WalletError>> + Send + 'static,
    T: Send + 'static,
{
    refuse_reentry()?;
    spawn(work)?.await.map_err(|_| WalletError::Closed)?
}

thread_local! {
    static IN_CALLBACK: Cell<u32> = const { Cell::new(0) };
}

/// Mark the current thread as running a synchronous host callback for `call`'s duration,
/// so a wallet call the host makes from inside it is refused instead of waiting for the
/// command that is waiting for the callback.
pub(crate) fn in_callback<T>(call: impl FnOnce() -> T) -> T {
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            IN_CALLBACK.with(|depth| depth.set(depth.get().saturating_sub(1)));
        }
    }
    IN_CALLBACK.with(|depth| depth.set(depth.get().saturating_add(1)));
    let _leave = Leave;
    call()
}

/// Refuse a wallet call made from inside a synchronous host callback on this thread.
pub(crate) fn refuse_reentry() -> Result<(), WalletError> {
    if IN_CALLBACK.with(Cell::get) > 0 {
        Err(WalletError::Reentrant)
    } else {
        Ok(())
    }
}
