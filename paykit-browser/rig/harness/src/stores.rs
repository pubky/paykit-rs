//! The stores of a run, the host's test wrapping key, the host's record of each store's App
//! ID, and `cleanup`.
//!
//! A run (`run`) has two stores, named from it: `harness-<run>-wallet` for the payer's Paykit
//! App and `harness-<run>-shop` for the payee's. Their names say nothing about an identity, as
//! `OpfsStore::open` asks.
//!
//! Each store also holds, beside the SDK's state, the App ID of the app that keeps its state
//! there, under the harness's own namespace ([`HOST_NAMESPACE`]), as a host remembers which
//! app it joined. The read-back after a reload loads each app from that, so neither a key nor
//! the rig's seed passes through JavaScript. The Switch client writes only its own namespace
//! (`client/<App ID>`), and the SDK only its own.

use std::sync::Arc;

use js_sys::{Array, Reflect, Uint8Array};
use paykit_browser::store::{OpfsStore, WrappingKey};
use pubky_switch_client::store::{MemoryStore, StateStore, WriteBatch};
use pubky_switch_client::StoreError;
use serde_json::json;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Crypto, CryptoKey};

use crate::failure::{Args, Failure, Outcome};

/// The apps of a run, each with a store of its own: the payer's wallet and the payee's shop.
pub const APPS: [&str; 2] = ["wallet", "shop"];

/// The harness's namespace in each store.
const HOST_NAMESPACE: &str = "harness";
/// The key of the App ID in [`HOST_NAMESPACE`].
const APP_ID_KEY: &[u8] = b"app_id";

/// **A test-only key, public in this source file: it protects nothing.** The raw bytes of the
/// host's wrapping key for `store: "host"`. Every start imports it again as a
/// non-extractable AES-GCM `CryptoKey`, kept in memory only, so the worker after a reload
/// holds the same key and no `CryptoKey` is ever stored in IndexedDB, which in Playwright's
/// WebKit on macOS would ask for Keychain access. A real host keeps its key secret, for
/// example behind its own unlock step.
const TEST_HOST_KEY: [u8; 32] = *b"paykit-browser-harness-test-key!";

/// Where a run keeps its apps' state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    /// `OpfsStore` with `WrappingKey::Origin`: the origin holds the wrapping key.
    Origin,
    /// `OpfsStore` with `WrappingKey::Host` and the test key [`TEST_HOST_KEY`].
    Host,
    /// The Switch client's `MemoryStore`, for an engine with no Origin Private File System:
    /// nothing outlives the worker, so there is no read-back.
    Memory,
}

impl StoreKind {
    /// The `store` argument: `origin`, `host` or `memory`.
    pub fn from_args(args: &Args) -> Result<Self, Failure> {
        match args.text("store")? {
            "origin" => Ok(StoreKind::Origin),
            "host" => Ok(StoreKind::Host),
            "memory" => Ok(StoreKind::Memory),
            _ => Err(Failure::bad_args(
                "store must be \"origin\", \"host\" or \"memory\"",
            )),
        }
    }

    /// The `store` argument this kind is read from.
    pub fn as_str(self) -> &'static str {
        match self {
            StoreKind::Origin => "origin",
            StoreKind::Host => "host",
            StoreKind::Memory => "memory",
        }
    }
}

/// The name of the store of `app` in `run`.
pub fn store_name(run: &str, app: &str) -> String {
    format!("harness-{run}-{app}")
}

/// Opens the store of `app` in `run`, of `kind`. A durable store must be opened in a
/// dedicated worker, and is closed when the last handle to it is dropped.
pub async fn open(kind: StoreKind, run: &str, app: &str) -> Result<Arc<dyn StateStore>, Failure> {
    let wrapping = match kind {
        StoreKind::Memory => return Ok(Arc::new(MemoryStore::new())),
        StoreKind::Origin => WrappingKey::Origin,
        StoreKind::Host => WrappingKey::Host(host_key().await?),
    };
    let store = OpfsStore::open(&store_name(run, app), wrapping)
        .await
        .map_err(|error| Failure::store_open(&format!("open the store of the {app}"), &error))?;
    Ok(Arc::new(store))
}

/// The App ID `store` holds in the harness's namespace, if any.
pub fn app_id(store: &dyn StateStore, app: &str) -> Result<Option<[u8; 16]>, Failure> {
    let held = store
        .get(HOST_NAMESPACE, APP_ID_KEY)
        .map_err(|error| Failure::store_call(&format!("read the App ID of the {app}"), &error))?;
    held.map(|bytes| {
        <[u8; 16]>::try_from(bytes.as_slice())
            .map_err(|_| Failure::store(format!("the App ID of the {app} is not 16 bytes")))
    })
    .transpose()
}

/// Keeps `app_id` in `store`, in the harness's namespace, in one commit.
pub fn keep_app_id(store: &dyn StateStore, app: &str, app_id: [u8; 16]) -> Result<(), Failure> {
    let mut batch = WriteBatch::new();
    batch
        .put(HOST_NAMESPACE, APP_ID_KEY.to_vec(), app_id.to_vec())
        .map_err(|error| {
            let error = StoreError::from(error);
            Failure::store_call(&format!("batch the App ID of the {app}"), &error)
        })?;
    store
        .commit(batch)
        .map_err(|error| Failure::store_call(&format!("keep the App ID of the {app}"), &error))
}

/// `cleanup`, with the argument `run`: destroys both stores of the run with
/// `OpfsStore::destroy`, their key records first. A store that does not exist is not an
/// error. Call it in a dedicated worker, once no worker holds the stores open.
///
/// Returns `{"destroyed": [<store name>, ...]}`.
pub async fn cleanup(args: &Args) -> Outcome {
    let run = args.run()?;
    let mut destroyed = Vec::new();
    for app in APPS {
        let name = store_name(run, app);
        OpfsStore::destroy(&name).await.map_err(|error| {
            Failure::store_open(&format!("destroy the store of the {app}"), &error)
        })?;
        destroyed.push(name);
    }
    Ok(json!({ "destroyed": destroyed }))
}

/// The host's wrapping key: [`TEST_HOST_KEY`] imported as a non-extractable AES-GCM key for
/// encrypting and decrypting, kept in memory only.
async fn host_key() -> Result<CryptoKey, Failure> {
    let refused = |step: &str| Failure::store(format!("the host's test key: {step}"));
    let crypto: Crypto = Reflect::get(&js_sys::global(), &JsValue::from_str("crypto"))
        .ok()
        .and_then(|value| value.dyn_into().ok())
        .ok_or_else(|| refused("the global scope has no crypto"))?;
    let usages = Array::of2(&JsValue::from_str("encrypt"), &JsValue::from_str("decrypt"));
    let raw = Uint8Array::from(&TEST_HOST_KEY[..]);
    let promise = crypto
        .subtle()
        .import_key_with_str("raw", &raw, "AES-GCM", false, &usages)
        .map_err(|_| refused("importKey throws"))?;
    JsFuture::from(promise)
        .await
        .map_err(|_| refused("importKey rejects"))?
        .dyn_into()
        .map_err(|_| refused("importKey gives no CryptoKey"))
}
