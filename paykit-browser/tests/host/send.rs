//! `run_local`: a future that waits on the browser, behind a `Send` future.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use js_sys::Promise;
use paykit_browser::send::{run_local, LocalTaskDropped};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::support::next_task;

/// Compiles only for a `Send + 'static` value.
fn assert_send<T: Send + 'static>(_: &T) {}

/// Sets its flag when dropped. The `Rc` makes any future that holds it not `Send`.
struct DropFlag(Rc<Cell<bool>>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

/// A local future that sets `started` when first polled, holds `guard`, and then waits on a
/// promise that never settles.
async fn wait_forever(started: Rc<Cell<bool>>, guard: DropFlag) {
    let _guard = guard;
    started.set(true);
    let _ = JsFuture::from(Promise::new(&mut |_resolve, _reject| {})).await;
}

/// The returned future resolves with the output of a local future that awaited a JavaScript
/// promise and held a `JsValue` and an `Rc` across the await.
#[wasm_bindgen_test]
async fn test_send_run_local_returns_the_output() {
    let local = Rc::new(7.0);
    let output = run_local(async move {
        let value: JsValue = JsFuture::from(Promise::resolve(&JsValue::from(35.0)))
            .await
            .unwrap();
        value.as_f64().unwrap() + *local
    })
    .await;
    assert_eq!(output, Ok(42.0));
}

/// The returned future is `Send` and `'static`, although the local future is neither `Send`
/// nor holds only `Send` values. This is checked when the test compiles.
#[wasm_bindgen_test]
async fn test_send_run_local_future_is_send() {
    let flag = Rc::new(Cell::new(false));
    let local = {
        let flag = flag.clone();
        async move {
            let value = JsFuture::from(Promise::resolve(&JsValue::TRUE))
                .await
                .unwrap();
            flag.set(value.is_truthy());
            flag.get()
        }
    };
    let returned = run_local(local);
    assert_send(&returned);
    assert_eq!(returned.await, Ok(true));
    assert!(flag.get());
}

/// The local future starts, in a later microtask, without the returned future being polled.
#[wasm_bindgen_test]
async fn test_send_run_local_starts_without_a_poll() {
    let started = Rc::new(Cell::new(false));
    let returned = {
        let started = started.clone();
        run_local(async move { started.set(true) })
    };
    next_task().await;
    assert!(started.get());
    assert_eq!(returned.await, Ok(()));
}

/// Dropping the returned future drops a local future that waits forever, and runs its
/// destructors; while the returned future lives, the local future is not dropped.
#[wasm_bindgen_test]
async fn test_send_run_local_drop_cancels_the_local_future() {
    let started = Rc::new(Cell::new(false));
    let dropped = Rc::new(Cell::new(false));
    let returned = run_local(wait_forever(started.clone(), DropFlag(dropped.clone())));
    next_task().await;
    assert!(started.get());
    assert!(!dropped.get());

    drop(returned);
    next_task().await;
    assert!(dropped.get());
}

/// A returned future dropped before its local future ever ran drops the local future without
/// polling it.
#[wasm_bindgen_test]
async fn test_send_run_local_drop_before_start_never_polls() {
    let started = Rc::new(Cell::new(false));
    let dropped = Rc::new(Cell::new(false));
    drop(run_local(wait_forever(
        started.clone(),
        DropFlag(dropped.clone()),
    )));
    next_task().await;
    assert!(!started.get());
    assert!(dropped.get());
}

/// The output of a local future that finished is dropped with the returned future when
/// nobody takes it, not leaked.
#[wasm_bindgen_test]
async fn test_send_run_local_unclaimed_output_is_dropped() {
    /// Sets its flag when dropped; `Send`, so it can be an output.
    struct Output(Arc<AtomicBool>);
    impl Drop for Output {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let returned = {
        let dropped = dropped.clone();
        run_local(async move { Output(dropped) })
    };
    next_task().await;
    assert!(!dropped.load(Ordering::SeqCst));
    drop(returned);
    assert!(dropped.load(Ordering::SeqCst));
}

/// The error a caller gets when the local future is dropped unfinished is a standard error
/// with a message that names what happened.
#[wasm_bindgen_test]
fn test_send_local_task_dropped_is_an_error() {
    let error: Box<dyn std::error::Error + Send + Sync> = Box::new(LocalTaskDropped);
    assert_eq!(
        error.to_string(),
        "the local task was dropped before it finished"
    );
}
