//! THROWAWAY (#411): debug and lifecycle hooks for the browser lifecycle
//! experiments (multi-tab, reload during private intake, frozen tabs, panic
//! recovery). None of this is a production API:
//!
//! - `exportIdentityForTest` hands the Pubky root secret and the grant
//!   session secret to JS as strings so a reloaded page, a second tab or a
//!   fresh worker can reopen the same identity. A real binding must not do
//!   this; where browser secrets live is an open product/security decision.
//! - `forceReleaseIntakeLease` deletes the identity-wide private intake
//!   supervisor lease whatever its owner. It is only sound while the caller
//!   holds an origin-wide Web Lock that every intake owner also holds, and
//!   only for device-local storage (IndexedDB): another device sharing the
//!   identity through Pubky shared state is invisible to Web Locks.
//! - The write-lock hooks talk to the homeserver directly to observe how a
//!   frozen holder behaves; they bypass SDK state.
//! - `debugPanic` exists to prove a panic kills the instance.

use std::{sync::Arc, time::Duration};

use paykit_sdk::{
    PaykitSdk, PaykitSdkConfig, PubkyLocalSecretKey, PubkySessionBootstrap, StorageAdapter,
    PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
};
use serde_json::json;
use wasm_bindgen::prelude::*;

use super::{
    flows::sdk_err, js_err, make_storage, JsBlobStorage, JsStateStore, PaykitWasmClient,
    WasmPaymentAdapter, WasmSessionProvider, CLIENT_ID,
};

thread_local! {
    static PANIC_IN_NEXT_TRANSACTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Consume the armed "panic in the next JS-store transaction" flag.
pub(super) fn take_transaction_panic() -> bool {
    PANIC_IN_NEXT_TRANSACTION.with(|flag| flag.replace(false))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write-lock test files live under the session's `/pub/paykit/` capability.
const LOCK_TEST_DIR: &str = "/pub/paykit/v0/spike-lifecycle/";

fn lock_test_path(name: &str) -> Result<String, JsError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(JsError::new("lock test name must be [A-Za-z0-9_-]{1,64}"));
    }
    Ok(format!("{LOCK_TEST_DIR}{name}"))
}

fn lease_clock_ms() -> f64 {
    paykit_lib::rt::lease_clock()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(f64::NAN)
}

fn pubky_status(err: &pubky::Error) -> Option<u16> {
    match err {
        pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }) => {
            Some(status.as_u16())
        }
        _ => None,
    }
}

/// Lease JSON from storage state, or `null`.
fn lease_json(state: &paykit_sdk::storage::StorageState) -> serde_json::Value {
    match &state.private_intake_supervisor_lease {
        None => serde_json::Value::Null,
        Some(lease) => json!({
            "leaseId": lease.lease_id,
            "claimedAt": lease.claimed_at.to_rfc3339(),
            "expiresAt": lease.expires_at.to_rfc3339(),
            "expiresInMs": (lease.expires_at - chrono::Utc::now()).num_milliseconds(),
        }),
    }
}

#[wasm_bindgen]
impl PaykitWasmClient {
    /// THROWAWAY (#411): `{secretKeyHex, sessionSecret}` for reopening this
    /// identity in another browsing context. Exposes the Pubky root secret
    /// and a bearer-equivalent grant secret to JS. Test-only.
    #[wasm_bindgen(js_name = exportIdentityForTest)]
    pub fn export_identity_for_test(&self) -> Result<String, JsError> {
        let secret = self
            .access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| JsError::new("no local secret key"))?;
        Ok(json!({
            "publicKey": self.public_key.as_str(),
            "appId": self.app_id.to_string(),
            "secretKeyHex": hex(secret.as_bytes()),
            "sessionSecret": self.provider.session_secret.as_str(),
        })
        .to_string())
    }

    /// THROWAWAY (#411): open an existing testnet identity in this browsing
    /// context and initialize an SDK runtime over `storage_mode`.
    ///
    /// With `session_secret` the exported grant is restored
    /// (`import_session`): pubky exchanges it for a fresh bearer, which
    /// invalidates the bearer of every other handle on the same grant. Without
    /// it the root key signs in again, which creates a separate grant whose
    /// bearer does not collide with other contexts.
    #[wasm_bindgen(js_name = openOnTestnet)]
    pub async fn open_on_testnet(
        app_id: String,
        secret_key_hex: String,
        session_secret: Option<String>,
        storage_mode: String,
        store: Option<JsStateStore>,
    ) -> Result<PaykitWasmClient, JsError> {
        let app_id = paykit_lib::PaykitAppId::new(app_id).map_err(|e| js_err("app id", e))?;
        let secret_key =
            PubkyLocalSecretKey::from_hex(&secret_key_hex).map_err(|e| js_err("secret key", e))?;
        let pubky = pubky::Pubky::testnet().map_err(|e| js_err("Pubky::testnet", e))?;
        let bootstrap = PubkySessionBootstrap::with_pubky(pubky, CLIENT_ID)
            .map_err(|e| js_err("bootstrap", e))?;
        let result = match &session_secret {
            Some(secret) => bootstrap
                .import_session(
                    secret,
                    Some(secret_key.clone()),
                    PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
                )
                .await
                .map_err(|e| sdk_err("import_session", e))?,
            None => bootstrap
                .sign_in(&secret_key, PAYKIT_AUTHORIZER_SESSION_CAPABILITIES)
                .await
                .map_err(|e| sdk_err("sign_in", e))?,
        };
        let session_secret = match session_secret {
            Some(secret) => secret,
            None => result
                .export_session_secret()
                .await
                .map_err(|e| sdk_err("export_session_secret", e))?
                .into_inner(),
        };
        let access = result.access;
        let adapter = WasmPaymentAdapter::default();
        let provider = WasmSessionProvider {
            access: Arc::new(std::sync::Mutex::new(Some(access.clone()))),
            session_secret: Arc::new(session_secret),
        };
        let storage = make_storage(&storage_mode, store, &provider)?;
        let config = PaykitSdkConfig::new(app_id.clone()).map_err(|e| js_err("config", e))?;
        let sdk = Arc::new(PaykitSdk::new(
            storage,
            provider.clone(),
            adapter.clone(),
            config,
        ));
        sdk.initialize()
            .await
            .map_err(|e| sdk_err("initialize (reopened identity)", e))?;
        Ok(PaykitWasmClient {
            sdk,
            adapter,
            provider,
            storage_mode,
            access,
            public_key: result.public_key,
            app_id,
            intake: None,
            deadline_ms: std::cell::Cell::new(None),
        })
    }

    /// THROWAWAY (#411): write `value` to a test file through the SDK's
    /// session with a plain (unlocked) PUT. Returns `{ok, status, error}`.
    /// Used to observe a 401 after another context restored the same grant.
    #[wasm_bindgen(js_name = debugPutTestFile)]
    pub async fn debug_put_test_file(
        &self,
        name: String,
        value: String,
    ) -> Result<String, JsError> {
        let path = lock_test_path(&name)?;
        Ok(match self
            .access
            .session
            .storage()
            .put(path.as_str(), value)
            .await
        {
            Ok(_) => json!({ "ok": true }),
            Err(e) => json!({ "ok": false, "status": pubky_status(&e), "error": e.to_string() }),
        }
        .to_string())
    }

    /// THROWAWAY (#411): read a test file (public read, no-store not set).
    #[wasm_bindgen(js_name = debugReadTestFile)]
    pub async fn debug_read_test_file(&self, name: String) -> Result<String, JsError> {
        let path = lock_test_path(&name)?;
        let response = self
            .access
            .session
            .storage()
            .get(path.as_str())
            .await
            .map_err(|e| js_err("get test file", e))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| js_err("read test file", e))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// THROWAWAY (#411): run Paykit's `with_write_lock` on a test file,
    /// hold the lock for `hold_ms` inside the operation, then PUT `value`
    /// under the lock. Returns a JSON timeline: lease-clock and wall-clock
    /// readings at acquire and after the hold, and the outcome. Freezing the
    /// page during the hold shows what the lease clock and `poll_with_lease`
    /// do when the holder resumes after its lease expired.
    #[wasm_bindgen(js_name = debugHoldWriteLock)]
    pub async fn debug_hold_write_lock(
        &self,
        name: String,
        hold_ms: u32,
        value: String,
    ) -> Result<String, JsError> {
        let path = lock_test_path(&name)?;
        let session = &self.access.session;
        let started = (lease_clock_ms(), js_sys::Date::now());
        let timeline = std::cell::RefCell::new(serde_json::Map::new());
        let result: Result<(), paykit_lib::PaykitError> =
            paykit_lib::with_write_lock(session, &path, |lock| {
                let timeline = &timeline;
                async move {
                    timeline.borrow_mut().insert(
                        "acquired".into(),
                        json!({ "leaseClockMs": lease_clock_ms(), "wallMs": js_sys::Date::now(), "grantedTimeoutMs": lock.timeout().as_millis() as u64 }),
                    );
                    paykit_lib::rt::sleep(Duration::from_millis(hold_ms.into())).await;
                    timeline.borrow_mut().insert(
                        "afterHold".into(),
                        json!({ "leaseClockMs": lease_clock_ms(), "wallMs": js_sys::Date::now() }),
                    );
                    let put = session.storage().put_locked(&lock, value).await;
                    timeline.borrow_mut().insert(
                        "put".into(),
                        match &put {
                            Ok(_) => json!({ "ok": true, "wallMs": js_sys::Date::now() }),
                            Err(e) => json!({ "ok": false, "status": pubky_status(e), "error": e.to_string() }),
                        },
                    );
                    put.map(|_| ()).map_err(|e| paykit_lib::PaykitError::Transport {
                        context: "put_locked".into(),
                        source: e.into(),
                    })
                }
            })
            .await;
        let mut out = timeline.into_inner();
        out.insert(
            "start".into(),
            json!({ "leaseClockMs": started.0, "wallMs": started.1 }),
        );
        out.insert(
            "end".into(),
            json!({ "leaseClockMs": lease_clock_ms(), "wallMs": js_sys::Date::now() }),
        );
        out.insert(
            "result".into(),
            match &result {
                Ok(()) => json!({ "ok": true }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            },
        );
        Ok(serde_json::Value::Object(out).to_string())
    }

    /// THROWAWAY (#411): raw pubky lock without Paykit's lease clock: take a
    /// `lock_secs` lock on a test file, wait `delay_ms`, then `put_locked`
    /// with the (possibly expired) token. Shows whether the homeserver itself
    /// fences a stale holder. Leaves the lock to expire.
    #[wasm_bindgen(js_name = debugRawLockedPut)]
    pub async fn debug_raw_locked_put(
        &self,
        name: String,
        lock_secs: u32,
        delay_ms: u32,
        value: String,
    ) -> Result<String, JsError> {
        let path = lock_test_path(&name)?;
        let storage = self.access.session.storage();
        let lock = storage
            .lock(path.as_str(), Duration::from_secs(lock_secs.into()))
            .await
            .map_err(|e| js_err("lock", e))?;
        let locked_at = js_sys::Date::now();
        paykit_lib::rt::sleep(Duration::from_millis(delay_ms.into())).await;
        let put_at = js_sys::Date::now();
        let put = storage.put_locked(&lock, value).await;
        Ok(json!({
            "grantedTimeoutMs": lock.timeout().as_millis() as u64,
            "heldForMs": put_at - locked_at,
            "put": match &put {
                Ok(_) => json!({ "ok": true }),
                Err(e) => json!({ "ok": false, "status": pubky_status(e), "error": e.to_string() }),
            },
        })
        .to_string())
    }

    /// THROWAWAY (#411): current lease-clock and clock readings.
    #[wasm_bindgen(js_name = debugClocks)]
    pub fn debug_clocks() -> String {
        json!({
            "leaseClockMs": lease_clock_ms(),
            "wallMs": js_sys::Date::now(),
        })
        .to_string()
    }

    /// THROWAWAY (#411): the private intake supervisor lease stored in a JS
    /// state store, or `null`.
    #[wasm_bindgen(js_name = debugIntakeLease)]
    pub async fn debug_intake_lease(store: JsStateStore) -> Result<String, JsError> {
        let storage = JsBlobStorage {
            store,
            serialize: tokio::sync::Mutex::new(()),
        };
        let value = StorageAdapter::transaction(&storage, |tx| {
            Ok(lease_json(&tx.export_storage_state()).to_string())
        })
        .await
        .map_err(|e| sdk_err("read intake lease", e))?;
        Ok(value)
    }

    /// THROWAWAY (#411): release the stored private intake supervisor lease
    /// whatever its lease ID. Sound only under the origin-wide Web Lock that
    /// every intake owner holds, and only for device-local storage (see the
    /// module docs). Returns the released lease or `null`.
    #[wasm_bindgen(js_name = forceReleaseIntakeLease)]
    pub async fn force_release_intake_lease(store: JsStateStore) -> Result<String, JsError> {
        let storage = JsBlobStorage {
            store,
            serialize: tokio::sync::Mutex::new(()),
        };
        let value = StorageAdapter::transaction(&storage, |tx| {
            let state = tx.export_storage_state();
            let released = lease_json(&state);
            if let Some(lease) = state.private_intake_supervisor_lease {
                tx.release_private_intake_supervisor(lease.lease_id);
            }
            Ok(released.to_string())
        })
        .await
        .map_err(|e| sdk_err("force release intake lease", e))?;
        Ok(value)
    }

    /// THROWAWAY (#411): whether this runtime's private intake supervisor has
    /// finished on its own (for example after a failed lease renewal).
    #[wasm_bindgen(js_name = privateIntakeFinished)]
    pub fn private_intake_finished(&self) -> Option<bool> {
        self.intake.as_ref().map(|s| s.is_finished())
    }

    /// THROWAWAY (#411): panic on purpose. `kind` is `"sync"` (panic on the
    /// first poll of this call), `"async"` (panic after an await inside this
    /// call), `"task"` (panic inside a task spawned with `rt::spawn`; this
    /// call itself returns) or `"transaction"` (panic inside a JS-store
    /// storage transaction while its async mutex guard is held).
    ///
    /// With `panic = "abort"` the panic is a wasm trap: no destructor runs,
    /// so whatever the panicking code held (mutex guards, RefCell borrows,
    /// the shadow stack it had used) stays held. The trap only unwinds the
    /// current JS-to-wasm call; the instance keeps answering other calls,
    /// which is what makes it dangerous rather than safe (see the #411
    /// lifecycle notes).
    #[wasm_bindgen(js_name = debugPanic)]
    pub async fn debug_panic(&self, kind: String) -> Result<(), JsError> {
        match kind.as_str() {
            "sync" => panic!(
                "THROWAWAY (#411): debug panic (sync) for {}",
                self.public_key.as_str()
            ),
            "async" => {
                paykit_lib::rt::sleep(Duration::from_millis(1)).await;
                panic!("THROWAWAY (#411): debug panic (async)");
            }
            "transaction" => {
                // Panic inside the next JS-store transaction, while
                // `JsBlobStorage` holds its async mutex guard.
                PANIC_IN_NEXT_TRANSACTION.with(|flag| flag.set(true));
                self.sdk
                    .contact_records()
                    .await
                    .map_err(|e| sdk_err("contact_records", e))?;
                Ok(())
            }
            "task" => {
                drop(paykit_lib::rt::spawn(async {
                    paykit_lib::rt::sleep(Duration::from_millis(1)).await;
                    panic!("THROWAWAY (#411): debug panic (spawned task)");
                }));
                paykit_lib::rt::sleep(Duration::from_millis(20)).await;
                Ok(())
            }
            other => Err(JsError::new(&format!("unknown panic kind {other:?}"))),
        }
    }
}
