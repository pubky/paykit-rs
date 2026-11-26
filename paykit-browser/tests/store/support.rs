//! What the browser tests of this target share beyond `tests/opfs/`: the slot header, listing
//! the OPFS root, the engine's lock error, and a nested dedicated worker.
//!
//! Listing the root and starting a nested worker go through small JavaScript functions, which
//! need no further `web-sys` bindings.

use js_sys::{Array, Function, Promise};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::DedicatedWorkerGlobalScope;

/// The offset of a slot's payload: after the magic, the format version, the generation and the
/// payload length (`src/store/format.rs`).
pub const PAYLOAD_AT: usize = 28;

fn scope() -> DedicatedWorkerGlobalScope {
    js_sys::global()
        .dyn_into()
        .expect("the store tests run in a dedicated worker")
}

/// Call a JavaScript function, built from `body` with the parameters `args`, that returns a
/// promise of a string, and await that string.
async fn call_js(args: &str, body: &str, values: &[&str]) -> String {
    let function = Function::new_with_args(args, body);
    let values: Array = values
        .iter()
        .map(|value| JsValue::from_str(value))
        .collect();
    let promise: Promise = function
        .apply(&JsValue::UNDEFINED, &values)
        .unwrap()
        .dyn_into()
        .unwrap();
    JsFuture::from(promise).await.unwrap().as_string().unwrap()
}

/// The generation in a slot's header.
pub fn generation(slot: &[u8]) -> u64 {
    assert_eq!(&slot[..8], b"PKBSTORE", "not a slot");
    u64::from_le_bytes(slot[12..20].try_into().unwrap())
}

/// The names in the origin's OPFS root, sorted.
pub async fn root_names() -> Vec<String> {
    const BODY: &str = r#"
return (async () => {
  const root = await navigator.storage.getDirectory();
  const names = [];
  for await (const name of root.keys()) names.push(name);
  return JSON.stringify(names.sort());
})();
"#;
    let names = call_js("", BODY, &[]).await;
    serde_json::from_str(&names).unwrap()
}

/// The browser engine, from the user agent.
pub fn engine() -> &'static str {
    let agent = scope().navigator().user_agent().unwrap();
    if agent.contains("Firefox/") {
        "firefox"
    } else if agent.contains("Chrome/") {
        "chromium"
    } else if agent.contains("AppleWebKit/") {
        "webkit"
    } else {
        panic!("unknown engine: {agent}")
    }
}

/// The name of the error the engine gives a second synchronous access handle on a locked file:
/// the spec's `NoModificationAllowedError` in Chromium and Firefox, `InvalidStateError` in
/// WebKit.
pub fn lock_error_name() -> &'static str {
    match engine() {
        "webkit" => "InvalidStateError",
        _ => "NoModificationAllowedError",
    }
}

/// Start a nested dedicated worker that takes a synchronous access handle on the file `file`
/// of the store `name`, closes it, and answers `ok`, or answers the name of the error it got.
/// An engine without nested workers, or a worker that does not answer within 10 seconds, gives
/// an answer that says so, which no test accepts.
pub async fn open_in_nested_worker(name: &str, file: &str) -> String {
    const BODY: &str = r#"
return new Promise((resolve) => {
  if (typeof Worker !== "function") {
    resolve("this engine has no nested dedicated workers: Worker is not defined in a worker");
    return;
  }
  const source = `
    onmessage = async (event) => {
      try {
        const root = await navigator.storage.getDirectory();
        const directory = await root.getDirectoryHandle(event.data[0]);
        const file = await directory.getFileHandle(event.data[1]);
        const handle = await file.createSyncAccessHandle();
        handle.close();
        postMessage("ok");
      } catch (error) {
        postMessage(error && error.name ? error.name : String(error));
      }
    };
  `;
  const url = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
  let worker;
  let timer;
  const done = (answer) => {
    clearTimeout(timer);
    if (worker) worker.terminate();
    URL.revokeObjectURL(url);
    resolve(answer);
  };
  timer = setTimeout(() => done("the nested dedicated worker did not answer within 10 s"), 10000);
  try {
    worker = new Worker(url);
  } catch (error) {
    done("this engine refused to start a nested dedicated worker: " + error.name);
    return;
  }
  worker.onmessage = (event) => done(String(event.data));
  worker.onerror = (event) => {
    event.preventDefault();
    done("the nested dedicated worker failed: " + event.message);
  };
  worker.postMessage([name, file]);
});
"#;
    call_js("name, file", BODY, &[name, file]).await
}
