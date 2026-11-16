//! THROWAWAY (#411): browser facade. See the crate docs.
//!
//! Security notes for reviewers (this is a spike, not a design):
//! - The Pubky root secret, the exported grant session secret and every SDK
//!   secret live in wasm linear memory for the lifetime of a client. Nothing
//!   here persists them; the JS-backed state store receives the SDK state blob
//!   (which contains Noise static and link secrets) as plaintext bytes.
//! - The facade talks to a local testnet only (`Pubky::testnet()`), over
//!   plain HTTP on localhost.

mod flows;
mod lifecycle;

pub use flows::PaykitWasmBackup;

use std::{
    any::Any,
    cell::Cell,
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use paykit_sdk::{
    storage::{decode_storage_state_blob, encode_storage_state_blob, StorageTransactionCallback},
    IdentityState, InMemoryStorage, LinkedPeerState, PaykitApp, PaykitAppCapabilities, PaykitAppId,
    PaykitSdk, PaykitSdkConfig, PaykitSdkError, PaymentAdapter, PaymentTarget, PrivateIntakeConfig,
    PrivateIntakeSupervisor, PrivatePaymentEndpointCandidate,
    PrivatePaymentEndpointSelectionRequest, PrivateReceivingDetail, PubkyLocalSecretKey,
    PubkyPublicKey, PubkySessionAccess, PubkySessionBootstrap, PubkySessionProvider,
    PubkySharedStateStorage, PublicPaymentEndpointCandidate, PublicPaymentEndpointSelectionRequest,
    PublicReceivingDetail, StorageAdapter, PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
};
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

const CLIENT_ID: &str = "paykit-wasm.spike";

#[wasm_bindgen(start)]
fn start() {
    // THROWAWAY (#411): without a hook a Rust panic is only
    // "RuntimeError: unreachable" in the console. With panic=abort the
    // instance is unusable after any panic either way.
    console_error_panic_hook::set_once();
}

fn js_err(context: &str, err: impl std::fmt::Display) -> JsError {
    JsError::new(&format!("{context}: {err}"))
}

fn parse_key(value: &str) -> Result<PubkyPublicKey, JsError> {
    PubkyPublicKey::new(value).map_err(|e| js_err("invalid Pubky public key", e))
}

// ---------------------------------------------------------------------------
// JS-backed state store (spike of the async storage callback shape)
// ---------------------------------------------------------------------------

#[wasm_bindgen]
extern "C" {
    /// JS object with `load(): Promise<{revision: string, bytes: Uint8Array} | null>`
    /// and `saveIfRevision(expected: string | null, bytes: Uint8Array): Promise<boolean>`.
    ///
    /// `saveIfRevision` must commit atomically only when the stored revision
    /// still equals `expected` and return `false` otherwise (compare-and-swap).
    #[wasm_bindgen(typescript_type = "PaykitStateStore")]
    pub type JsStateStore;

    #[wasm_bindgen(method, catch)]
    async fn load(this: &JsStateStore) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = saveIfRevision)]
    async fn save_if_revision(
        this: &JsStateStore,
        expected: JsValue,
        bytes: js_sys::Uint8Array,
    ) -> Result<JsValue, JsValue>;
}

fn storage_err(context: &str, err: JsValue) -> PaykitSdkError {
    PaykitSdkError::Storage {
        context: format!("{context}: {err:?}"),
        source: None,
    }
}

/// `StorageAdapter` whose durable backing is an async JS object (for example
/// IndexedDB). Each transaction loads the whole blob, runs the synchronous SDK
/// callback in memory and writes back with a revision compare-and-swap.
///
/// `JsStateStore` wraps a `JsValue`, which is `Send + Sync` on non-atomics
/// wasm32, so the struct satisfies `StorageAdapter: Send + Sync`. The futures
/// it awaits (`JsFuture`) are `!Send`, which only works because the SDK's
/// traits use `async_trait(?Send)` on wasm32.
pub struct JsBlobStorage {
    store: JsStateStore,
    // THROWAWAY (#411): async mutex, because a contended std Mutex cannot block
    // on the browser main thread.
    serialize: tokio::sync::Mutex<()>,
}

impl JsBlobStorage {
    async fn transaction(
        &self,
        f: StorageTransactionCallback<'_>,
    ) -> paykit_sdk::Result<Box<dyn Any + Send>> {
        let _guard = self.serialize.lock().await;
        let loaded = self
            .store
            .load()
            .await
            .map_err(|e| storage_err("load state blob", e))?;
        let (revision, before_bytes) = if loaded.is_null() || loaded.is_undefined() {
            (JsValue::NULL, None)
        } else {
            let revision = js_sys::Reflect::get(&loaded, &"revision".into())
                .map_err(|e| storage_err("read revision", e))?;
            let bytes = js_sys::Reflect::get(&loaded, &"bytes".into())
                .map_err(|e| storage_err("read bytes", e))?;
            let bytes = js_sys::Uint8Array::new(&bytes).to_vec();
            (revision, Some(bytes))
        };
        let state = match &before_bytes {
            Some(bytes) => decode_storage_state_blob(bytes)?,
            None => Default::default(),
        };
        // THROWAWAY (#411): panic-recovery experiment hook (see
        // `lifecycle::arm_transaction_panic`): panic while `_guard` is held,
        // as a bug inside a storage transaction would.
        if lifecycle::take_transaction_panic() {
            panic!("THROWAWAY (#411): debug panic inside a storage transaction");
        }
        let (updated, value) = paykit_sdk::storage::run_storage_state_transaction(state, f)?;
        let after_bytes = encode_storage_state_blob(&updated)?;
        // THROWAWAY (#411): change detection by re-encoding; a real adapter
        // would track dirtiness instead of serializing the whole state twice.
        let changed = match &before_bytes {
            Some(before) => {
                let canonical_before =
                    encode_storage_state_blob(&decode_storage_state_blob(before)?)?;
                canonical_before != after_bytes
            }
            None => after_bytes != encode_storage_state_blob(&Default::default())?,
        };
        if changed {
            let committed = self
                .store
                .save_if_revision(revision, js_sys::Uint8Array::from(after_bytes.as_slice()))
                .await
                .map_err(|e| storage_err("save state blob", e))?;
            if committed.as_bool() != Some(true) {
                return Err(PaykitSdkError::ConcurrentUpdate {
                    context: "JS state store revision changed".into(),
                    source: None,
                });
            }
        }
        Ok(value)
    }
}

#[async_trait(?Send)]
impl StorageAdapter for JsBlobStorage {
    async fn load_local_identity_state(&self) -> paykit_sdk::Result<Option<IdentityState>> {
        self.load_identity_state().await
    }

    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> paykit_sdk::Result<Box<dyn Any + Send>> {
        self.transaction(f).await
    }
}

// ---------------------------------------------------------------------------
// Session provider and payment adapter
// ---------------------------------------------------------------------------

/// Holds the live session in memory, like the native test harness.
#[derive(Clone)]
pub struct WasmSessionProvider {
    access: Arc<Mutex<Option<PubkySessionAccess>>>,
    // THROWAWAY (#411): grant restore material kept in wasm memory so sign-out
    // can revoke the grant. A real browser binding must decide where (or
    // whether) this bearer-equivalent secret lives.
    session_secret: Arc<String>,
}

#[async_trait(?Send)]
impl PubkySessionProvider for WasmSessionProvider {
    async fn load_session_access(&self) -> paykit_sdk::Result<Option<PubkySessionAccess>> {
        Ok(self.access.lock().expect("session lock").clone())
    }

    async fn revoke_session_access(&self, access: &PubkySessionAccess) -> paykit_sdk::Result<()> {
        PubkySessionBootstrap::with_pubky(access.outbox_client.clone(), CLIENT_ID)?
            .revoke_grant(&self.session_secret, access)
            .await
    }

    async fn load_public_storage(&self) -> paykit_sdk::Result<Option<pubky::PublicStorage>> {
        Ok(self
            .access
            .lock()
            .expect("session lock")
            .as_ref()
            .map(|access| access.outbox_client.public_storage()))
    }

    async fn clear_session_access(&self) -> paykit_sdk::Result<()> {
        *self.access.lock().expect("session lock") = None;
        Ok(())
    }
}

/// Payment adapter whose receiving details JS sets directly.
#[derive(Clone, Default)]
pub struct WasmPaymentAdapter {
    public_details: Arc<Mutex<Vec<PublicReceivingDetail>>>,
    private_details: Arc<Mutex<Vec<PrivateReceivingDetail>>>,
}

#[async_trait(?Send)]
impl PaymentAdapter for WasmPaymentAdapter {
    async fn current_public_receiving_details(
        &self,
    ) -> paykit_sdk::Result<Vec<PublicReceivingDetail>> {
        Ok(self.public_details.lock().expect("details lock").clone())
    }

    async fn current_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
    ) -> paykit_sdk::Result<Vec<PrivateReceivingDetail>> {
        Ok(self.private_details.lock().expect("details lock").clone())
    }

    async fn select_public_payment_endpoints(
        &self,
        request: &PublicPaymentEndpointSelectionRequest,
    ) -> paykit_sdk::Result<Vec<PublicPaymentEndpointCandidate>> {
        Ok(request.candidates.clone())
    }

    async fn build_public_payment_target(
        &self,
        endpoint: &PublicPaymentEndpointCandidate,
    ) -> paykit_sdk::Result<PaymentTarget> {
        Ok(PaymentTarget {
            payload: endpoint.payload.clone(),
        })
    }

    async fn select_private_payment_endpoints(
        &self,
        request: &PrivatePaymentEndpointSelectionRequest,
    ) -> paykit_sdk::Result<Vec<PrivatePaymentEndpointCandidate>> {
        Ok(request.candidates.clone())
    }

    async fn build_private_payment_target(
        &self,
        endpoint: &PrivatePaymentEndpointCandidate,
    ) -> paykit_sdk::Result<PaymentTarget> {
        Ok(PaymentTarget {
            payload: endpoint.payload.clone(),
        })
    }
}

fn parse_details(json_text: &str) -> Result<Vec<(String, String)>, JsError> {
    let map: BTreeMap<String, String> =
        serde_json::from_str(json_text).map_err(|e| js_err("details must be {id: payload}", e))?;
    Ok(map.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Client facade
// ---------------------------------------------------------------------------

/// `Box<dyn StorageAdapter>` (the SDK implements the trait for boxes) lets
/// one client type pick in-memory, JS-backed or Pubky shared-state storage.
pub(crate) type Sdk = PaykitSdk<Box<dyn StorageAdapter>, WasmSessionProvider, WasmPaymentAdapter>;

/// One signed-up testnet identity with an initialized Paykit SDK runtime.
#[wasm_bindgen]
pub struct PaykitWasmClient {
    sdk: Arc<Sdk>,
    adapter: WasmPaymentAdapter,
    provider: WasmSessionProvider,
    storage_mode: String,
    access: PubkySessionAccess,
    public_key: PubkyPublicKey,
    app_id: PaykitAppId,
    intake: Option<PrivateIntakeSupervisor>,
    /// THROWAWAY (#411): facade-level call deadline (see `with_deadline`).
    deadline_ms: Cell<Option<u32>>,
}

/// Build the storage backing for `mode` (see `signUpOnTestnet`).
fn make_storage(
    mode: &str,
    store: Option<JsStateStore>,
    provider: &WasmSessionProvider,
) -> Result<Box<dyn StorageAdapter>, JsError> {
    Ok(match (mode, store) {
        ("memory", None) => Box::new(InMemoryStorage::new()),
        ("js", Some(store)) => Box::new(JsBlobStorage {
            store,
            serialize: tokio::sync::Mutex::new(()),
        }),
        ("shared", None) => Box::new(PubkySharedStateStorage::new(provider.clone())),
        (mode, store) => {
            return Err(JsError::new(&format!(
                "unsupported storage mode {mode:?} (store given: {})",
                store.is_some()
            )))
        }
    })
}

#[wasm_bindgen]
impl PaykitWasmClient {
    /// Sign up a fresh random keypair on the local testnet homeserver through
    /// the SDK's own bootstrap, then initialize the SDK, publish the Paykit
    /// Noise key authorization and register the Paykit app.
    ///
    /// `storage_mode` is `"memory"` (`InMemoryStorage`), `"js"` (the
    /// JS-backed blob store passed as `store`) or `"shared"`
    /// (`PubkySharedStateStorage`: encrypted state on the homeserver behind
    /// WebDAV write locks).
    #[wasm_bindgen(js_name = signUpOnTestnet)]
    pub async fn sign_up_on_testnet(
        app_id: String,
        homeserver: String,
        auth_relay_inbox: String,
        storage_mode: String,
        store: Option<JsStateStore>,
    ) -> Result<PaykitWasmClient, JsError> {
        let app_id = PaykitAppId::new(app_id).map_err(|e| js_err("app id", e))?;
        let homeserver = parse_key(&homeserver)?;
        let pubky = pubky::Pubky::testnet().map_err(|e| js_err("Pubky::testnet", e))?;
        let bootstrap = PubkySessionBootstrap::with_pubky(pubky, CLIENT_ID)
            .and_then(|b| b.with_auth_relay(&auth_relay_inbox))
            .map_err(|e| js_err("bootstrap", e))?;
        let keypair = pubky::Keypair::random();
        let secret_key = PubkyLocalSecretKey::new(keypair.secret_key());
        let result = bootstrap
            .sign_up(
                &secret_key,
                &homeserver,
                None,
                PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
            )
            .await
            .map_err(|e| js_err("sign_up", e))?;
        let session_secret = result
            .export_session_secret()
            .await
            .map_err(|e| js_err("export_session_secret", e))?
            .into_inner();
        let access = result.access;
        let adapter = WasmPaymentAdapter::default();
        let provider = WasmSessionProvider {
            access: Arc::new(Mutex::new(Some(access.clone()))),
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
        let status = sdk
            .initialize()
            .await
            .map_err(|e| js_err("initialize", e))?;
        if status.capability != paykit_sdk::PubkyIdentityCapability::PrivateLinkCapable {
            return Err(JsError::new(&format!(
                "unexpected capability after initialize: {:?}",
                status.capability
            )));
        }
        sdk.publish_paykit_noise_key_authorization()
            .await
            .map_err(|e| js_err("publish_paykit_noise_key_authorization", e))?;
        sdk.publish_paykit_app(
            PaykitApp::new(
                "Paykit wasm spike",
                PaykitAppCapabilities {
                    private_payments: true,
                    payment_requests: true,
                    receipts: true,
                    outgoing_payments: true,
                },
            )
            .map_err(|e| js_err("PaykitApp::new", e))?,
        )
        .await
        .map_err(|e| js_err("publish_paykit_app", e))?;
        Ok(PaykitWasmClient {
            sdk,
            adapter,
            provider,
            storage_mode,
            access,
            public_key: result.public_key,
            app_id,
            intake: None,
            deadline_ms: Cell::new(None),
        })
    }

    /// Build a second, fresh SDK runtime for this identity over a new
    /// `PubkySharedStateStorage` instance (only for `"shared"` mode), as an
    /// app restart would, and initialize it. Proves the encrypted state
    /// round-trips through the homeserver.
    #[wasm_bindgen(js_name = reopenSharedState)]
    pub async fn reopen_shared_state(&self) -> Result<PaykitWasmClient, JsError> {
        if self.storage_mode != "shared" {
            return Err(JsError::new(
                "reopenSharedState needs storage mode \"shared\"",
            ));
        }
        let storage: Box<dyn StorageAdapter> =
            Box::new(PubkySharedStateStorage::new(self.provider.clone()));
        let config = PaykitSdkConfig::new(self.app_id.clone()).map_err(|e| js_err("config", e))?;
        let sdk = Arc::new(PaykitSdk::new(
            storage,
            self.provider.clone(),
            self.adapter.clone(),
            config,
        ));
        sdk.initialize()
            .await
            .map_err(|e| js_err("initialize (reopened)", e))?;
        Ok(self.sibling(sdk, &self.storage_mode))
    }

    /// Linked Peer states as `{publicKey: state}` JSON.
    #[wasm_bindgen(js_name = linkedPeers)]
    pub async fn linked_peers(&self) -> Result<String, JsError> {
        let peers = self
            .sdk
            .linked_peers()
            .await
            .map_err(|e| js_err("linked_peers", e))?;
        let map: BTreeMap<String, String> = peers
            .into_iter()
            .map(|p| (p.counterparty.as_str().to_owned(), format!("{:?}", p.state)))
            .collect();
        Ok(serde_json::to_string(&map).expect("map serializes"))
    }

    /// z-base32 Pubky public key of this identity.
    #[wasm_bindgen(getter, js_name = publicKey)]
    pub fn public_key(&self) -> String {
        self.public_key.as_str().to_owned()
    }

    /// Paykit App ID of this runtime.
    #[wasm_bindgen(getter, js_name = appId)]
    pub fn app_id(&self) -> String {
        self.app_id.to_string()
    }

    /// Set the public receiving details (`{identifier: payload}` JSON).
    #[wasm_bindgen(js_name = setPublicDetails)]
    pub fn set_public_details(&self, details_json: &str) -> Result<(), JsError> {
        *self.adapter.public_details.lock().expect("details lock") = parse_details(details_json)?
            .into_iter()
            .map(|(identifier, payload)| PublicReceivingDetail {
                identifier,
                payload,
            })
            .collect();
        Ok(())
    }

    /// Set the private receiving details (`{identifier: payload}` JSON).
    #[wasm_bindgen(js_name = setPrivateDetails)]
    pub fn set_private_details(&self, details_json: &str) -> Result<(), JsError> {
        *self.adapter.private_details.lock().expect("details lock") = parse_details(details_json)?
            .into_iter()
            .map(|(identifier, payload)| PrivateReceivingDetail {
                identifier,
                payload,
            })
            .collect();
        Ok(())
    }

    /// `sync_public_endpoints`; returns `{published, removed, failed}` counts.
    #[wasm_bindgen(js_name = syncPublicEndpoints)]
    pub async fn sync_public_endpoints(&self) -> Result<String, JsError> {
        let report = self
            .with_deadline("sync_public_endpoints", async {
                self.sdk
                    .sync_public_endpoints()
                    .await
                    .map_err(|e| flows::sdk_err("sync_public_endpoints", e))
            })
            .await?;
        Ok(json!({
            "published": report.published.len(),
            "removed": report.removed.len(),
            "failed": report.failed.len(),
        })
        .to_string())
    }

    /// Read a payee's public Payment List for one app with paykit-lib's
    /// unauthenticated reader. Returns `{identifier: payload}` JSON.
    #[wasm_bindgen(js_name = readPublicPaymentList)]
    pub async fn read_public_payment_list(
        &self,
        payee: String,
        app_id: String,
    ) -> Result<String, JsError> {
        let payee = parse_key(&payee)?
            .to_public_key()
            .map_err(|e| js_err("payee", e))?;
        let app_id = PaykitAppId::new(app_id).map_err(|e| js_err("app id", e))?;
        let storage = self.access.outbox_client.public_storage();
        let list = self
            .with_deadline("get_payment_list", async {
                paykit_lib::get_payment_list(&storage, &payee, &app_id)
                    .await
                    .map_err(|e| js_err("get_payment_list", e))
            })
            .await?;
        let map: BTreeMap<String, String> = list
            .payment_endpoints
            .iter()
            .map(|(id, payload)| (id.as_str().to_owned(), payload.as_str().to_owned()))
            .collect();
        Ok(serde_json::to_string(&map).expect("map serializes"))
    }

    /// `resolve_public_contact_payment`; returns status and target payloads.
    #[wasm_bindgen(js_name = resolvePublicContactPayment)]
    pub async fn resolve_public_contact_payment(&self, payee: String) -> Result<String, JsError> {
        let payee = parse_key(&payee)?;
        let resolution = self
            .with_deadline("resolve_public_contact_payment", async {
                self.sdk
                    .resolve_public_contact_payment(payee, None)
                    .await
                    .map_err(|e| flows::sdk_err("resolve_public_contact_payment", e))
            })
            .await?;
        let payloads: Vec<&str> = resolution
            .payable_endpoints
            .iter()
            .map(|endpoint| endpoint.target.payload.as_str())
            .collect();
        Ok(json!({
            "status": format!("{:?}", resolution.status),
            "payloads": payloads,
            "failures": resolution.failures.len(),
        })
        .to_string())
    }

    /// Start the Encrypted Link Handshake as initiator.
    #[wasm_bindgen(js_name = initiateLink)]
    pub async fn initiate_link(&self, peer: String) -> Result<String, JsError> {
        let record = self
            .sdk
            .initiate_link_with_peer(parse_key(&peer)?)
            .await
            .map_err(|e| js_err("initiate_link_with_peer", e))?;
        Ok(format!("{:?}", record.state))
    }

    /// Accept the Encrypted Link Handshake as responder.
    #[wasm_bindgen(js_name = acceptLink)]
    pub async fn accept_link(&self, peer: String) -> Result<String, JsError> {
        let record = self
            .sdk
            .accept_link_with_peer(parse_key(&peer)?)
            .await
            .map_err(|e| js_err("accept_link_with_peer", e))?;
        Ok(format!("{:?}", record.state))
    }

    /// One Noise XX step; returns the resulting Linked Peer state.
    #[wasm_bindgen(js_name = advanceLink)]
    pub async fn advance_link(&self, peer: String) -> Result<String, JsError> {
        let record = self
            .sdk
            .advance_link_handshake(parse_key(&peer)?)
            .await
            .map_err(|e| js_err("advance_link_handshake", e))?;
        Ok(match record.state {
            LinkedPeerState::Linked => "Linked".into(),
            other => format!("{other:?}"),
        })
    }

    /// Queue the current Private Payment List for `peer` and process the
    /// outbound queue. Returns `{sent, failed}` counts.
    #[wasm_bindgen(js_name = sendPrivatePaymentList)]
    pub async fn send_private_payment_list(&self, peer: String) -> Result<String, JsError> {
        let peer = parse_key(&peer)?;
        self.sdk
            .enqueue_private_payment_list(peer.clone())
            .await
            .map_err(|e| js_err("enqueue_private_payment_list", e))?;
        let report = self
            .sdk
            .process_outbound_private_messages(peer)
            .await
            .map_err(|e| js_err("process_outbound_private_messages", e))?;
        Ok(json!({ "sent": report.sent.len(), "failed": report.failed.len() }).to_string())
    }

    /// Pull private messages from `peer`; returns the number of stored items.
    #[wasm_bindgen(js_name = receivePrivateMessages)]
    pub async fn receive_private_messages(&self, peer: String) -> Result<u32, JsError> {
        let report = self
            .sdk
            .receive_private_messages(parse_key(&peer)?)
            .await
            .map_err(|e| js_err("receive_private_messages", e))?;
        Ok(report.stream_item_ids.len() as u32)
    }

    /// Current Private Payment Lists received from `peer`, as
    /// `[{appId, endpoints: {identifier: payload}}]` JSON.
    #[wasm_bindgen(js_name = currentPrivatePaymentLists)]
    pub async fn current_private_payment_lists(&self, peer: String) -> Result<String, JsError> {
        let views = self
            .sdk
            .current_private_payment_lists(&parse_key(&peer)?)
            .await
            .map_err(|e| js_err("current_private_payment_lists", e))?;
        let out: Vec<Value> = views
            .into_iter()
            .map(|view| {
                let endpoints: BTreeMap<String, String> =
                    view.payment_endpoints.into_iter().collect();
                json!({ "appId": view.app_id.to_string(), "endpoints": endpoints })
            })
            .collect();
        Ok(Value::Array(out).to_string())
    }

    /// Start the private intake supervisor (live event streams, spawned
    /// tasks and timers) for this runtime.
    #[wasm_bindgen(js_name = startPrivateIntake)]
    pub async fn start_private_intake(&mut self) -> Result<(), JsError> {
        let config = PrivateIntakeConfig::new(1).map_err(|e| js_err("intake config", e))?;
        let supervisor = self
            .sdk
            .start_private_intake(config)
            .await
            .map_err(|e| js_err("start_private_intake", e))?;
        self.intake = Some(supervisor);
        Ok(())
    }

    /// Private intake health as JSON, or `null` when not running.
    #[wasm_bindgen(js_name = privateIntakeHealth)]
    pub async fn private_intake_health(&self) -> String {
        match &self.intake {
            None => "null".into(),
            Some(supervisor) => {
                let health = supervisor.health().await;
                let peers: BTreeMap<String, String> = health
                    .peers
                    .iter()
                    .map(|(k, v)| (k.as_str().to_owned(), format!("{v:?}")))
                    .collect();
                json!({
                    "finished": supervisor.is_finished(),
                    "reservedStreams": health.reserved_streams,
                    "peers": peers,
                    "audited": health.last_successful_audit_at.len(),
                })
                .to_string()
            }
        }
    }

    /// Process ciphertext that private intake already buffered for `peer`
    /// (no network read of new slots). Returns `{items, remaining, transport}`.
    #[wasm_bindgen(js_name = receiveBufferedPrivateMessages)]
    pub async fn receive_buffered_private_messages(&self, peer: String) -> Result<String, JsError> {
        let supervisor = self
            .intake
            .as_ref()
            .ok_or_else(|| JsError::new("private intake is not running"))?;
        let report = self
            .sdk
            .receive_buffered_private_messages(parse_key(&peer)?, supervisor)
            .await
            .map_err(|e| js_err("receive_buffered_private_messages", e))?;
        Ok(json!({
            "items": report.report.stream_item_ids.len(),
            "remaining": report.remaining_buffered_packets,
            "transport": format!("{:?}", report.freshness.transport_state),
        })
        .to_string())
    }

    /// Diagnostic: subscribe to the same live, path-filtered homeserver event
    /// stream shape private intake uses, for `user` and `path`, and collect
    /// events until `max_events` arrive or `timeout_ms` passes.
    #[wasm_bindgen(js_name = debugLiveStream)]
    pub async fn debug_live_stream(
        &self,
        user: String,
        path: String,
        max_events: u32,
        timeout_ms: u32,
    ) -> Result<String, JsError> {
        use futures_util::StreamExt;
        let user = parse_key(&user)?
            .to_public_key()
            .map_err(|e| js_err("user", e))?;
        let client = self.access.outbox_client.client().clone();
        let homeserver = self
            .access
            .outbox_client
            .get_homeserver_of(&user)
            .await
            .map_err(|e| js_err("get_homeserver_of", e))?
            .ok_or_else(|| JsError::new("no homeserver"))?;
        let mut stream = pubky::EventStreamBuilder::for_homeserver(client, &homeserver)
            .add_users([(&user, None)])
            .map_err(|e| js_err("add_users", e))?
            .path(path)
            .live()
            .subscribe()
            .await
            .map_err(|e| js_err("subscribe", e))?;
        let mut seen = Vec::new();
        let subscribed_at = paykit_lib::rt::Instant::now();
        let collect = async {
            while let Some(item) = stream.next().await {
                let at = subscribed_at.elapsed().as_millis();
                seen.push(match item {
                    Ok(event) => format!("+{at}ms {event:?}"),
                    Err(e) => format!("+{at}ms ERR {e}"),
                });
                if seen.len() >= max_events as usize {
                    break;
                }
            }
        };
        let _ = paykit_lib::rt::timeout(Duration::from_millis(timeout_ms.into()), collect).await;
        Ok(serde_json::to_string(&seen).expect("strings serialize"))
    }

    /// Stop the private intake supervisor if it is running.
    #[wasm_bindgen(js_name = stopPrivateIntake)]
    pub async fn stop_private_intake(&mut self) {
        if let Some(supervisor) = self.intake.take() {
            supervisor.stop().await;
        }
    }

    /// SHA fingerprint of the SDK backup state.
    #[wasm_bindgen(js_name = backupFingerprint)]
    pub async fn backup_fingerprint(&self) -> Result<String, JsError> {
        let backup = self
            .sdk
            .export_backup_state()
            .await
            .map_err(|e| js_err("export_backup_state", e))?;
        backup
            .content_fingerprint()
            .map_err(|e| js_err("content_fingerprint", e))
    }

    /// Sign out: revoke the grant on the homeserver and clear local identity state.
    #[wasm_bindgen(js_name = signOut)]
    pub async fn sign_out(&self) -> Result<String, JsError> {
        let status = self
            .sdk
            .sign_out()
            .await
            .map_err(|e| js_err("sign_out", e))?;
        Ok(format!("{:?}", status.capability))
    }
}

/// Resolve after `ms` milliseconds using the SDK's portable timer shim.
#[wasm_bindgen]
pub async fn sleep_ms(ms: u32) {
    paykit_lib::rt::sleep(Duration::from_millis(ms.into())).await;
}
