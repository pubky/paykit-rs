//! What the host tests share: the browser's monotonic clock, polling by hand, waiting for the
//! event loop, and a spy on `setTimeout`.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use js_sys::{Function, Promise, Reflect};
use serde_json::Value;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

// Bound in a different shape from the crate's own bindings: an import identical to one of the
// crate's would give the test binary a second definition of the same symbol.
#[wasm_bindgen]
extern "C" {
    type Performance;

    #[wasm_bindgen(thread_local_v2, js_name = performance)]
    static PERFORMANCE: Performance;

    #[wasm_bindgen(method)]
    fn now(this: &Performance) -> f64;
}

/// The browser's monotonic clock, `performance.now()`, in milliseconds.
pub fn performance_now() -> f64 {
    PERFORMANCE.with(Performance::now)
}

/// Poll `future` once, with a waker that does nothing.
pub fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Wait for the event loop's next task, a `setTimeout` of zero. Every microtask queued before
/// it has run by then, including the tasks `spawn_local` started.
pub async fn next_task() {
    let set_timeout: Function = Reflect::get(&js_sys::global(), &"setTimeout".into())
        .unwrap()
        .unchecked_into();
    let promise = Promise::new(&mut |resolve, _reject| {
        set_timeout
            .call2(&JsValue::UNDEFINED, &resolve, &0.into())
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

/// Replaces the global `setTimeout` and `clearTimeout`, and returns an object with
/// `timeouts()` (the recorded timeouts, as JSON, in the order they were set) and `restore()`.
/// Only timeouts with a delay from `min` to `max` ms are recorded and scaled; others pass
/// through unchanged.
const SPY: &str = r#"
const realSet = globalThis.setTimeout;
const realClear = globalThis.clearTimeout;
const timeouts = new Map();
globalThis.setTimeout = function (handler, ms, ...rest) {
  if (typeof handler !== "function" || !(ms >= min && ms <= max)) {
    return realSet.call(globalThis, handler, ms, ...rest);
  }
  const record = { ms, fired: false, cleared: false };
  const id = realSet.call(globalThis, (...args) => {
    record.fired = true;
    handler(...args);
  }, Math.floor(ms * scale), ...rest);
  timeouts.set(id, record);
  return id;
};
globalThis.clearTimeout = function (id) {
  const record = timeouts.get(id);
  if (record) record.cleared = true;
  return realClear.call(globalThis, id);
};
return {
  timeouts: () => JSON.stringify([...timeouts.values()]),
  restore: () => {
    globalThis.setTimeout = realSet;
    globalThis.clearTimeout = realClear;
  },
};
"#;

/// A spy on the global `setTimeout` and `clearTimeout` of this page or worker, for timeouts
/// with a delay from `min_ms` to `max_ms`: it records whether each one fired or was cleared,
/// and fires it after `scale` times its delay, so a `scale` below 1 makes those timers fire
/// early. Dropping it restores the real functions.
///
/// The tests run one at a time, so no other test sees the spy.
pub struct TimeoutSpy(JsValue);

impl TimeoutSpy {
    pub fn install(min_ms: u32, max_ms: u32, scale: f64) -> Self {
        let install = Function::new_with_args("min, max, scale", SPY);
        let args = js_sys::Array::of3(&min_ms.into(), &max_ms.into(), &scale.into());
        Self(install.apply(&JsValue::UNDEFINED, &args).unwrap())
    }

    /// The recorded timeouts in the order they were set, each with `ms` (the delay asked
    /// for), `fired` and `cleared`.
    pub fn timeouts(&self) -> Vec<Value> {
        let json = self.call("timeouts").as_string().unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn call(&self, name: &str) -> JsValue {
        let method: Function = Reflect::get(&self.0, &name.into())
            .unwrap()
            .unchecked_into();
        method.call0(&self.0).unwrap()
    }
}

impl Drop for TimeoutSpy {
    fn drop(&mut self) {
        self.call("restore");
    }
}
