uniffi::setup_scaffolding!();

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use once_cell::sync::OnceCell;
#[cfg(feature = "dev-auth")]
use pubky::Keypair;
use pubky::{Pubky, PubkySession, PublicKey};
use tokio::runtime::Runtime;
use tokio::sync::Mutex as TokioMutex;

use paykit_lib::{
    EncryptedLink, EncryptedLinkHandshake, EncryptedLinkHandshakeSnapshot, EncryptedLinkSnapshot,
    EndpointData, HandshakeProgress, MethodId, PaymentReference, PrivatePaymentsPayload,
    PubkyAuthenticatedTransport, PubkyUnauthenticatedTransport,
};

// ---------------------------------------------------------------------------
// Android logger — routes tracing/log output to logcat
// ---------------------------------------------------------------------------

#[cfg(target_os = "android")]
fn init_android_logger() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Debug)
                .with_tag("PaykitRust"),
        );
    });
}

/// Initialize Android-specific runtime hooks required by native dependencies.
///
/// Must be called from Android with an application `Context` before any Pubky
/// networking occurs so rustls-platform-verifier can call Android's certificate
/// verifier through the JVM.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_synonym_paykit_PaykitAndroid_nativeInitialize(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    context: jni::objects::JObject,
) -> jni::sys::jboolean {
    init_android_logger();

    match rustls_platform_verifier::android::init_with_env(&mut env, context) {
        Ok(()) => jni::sys::JNI_TRUE,
        Err(err) => {
            log::error!("Failed to initialize rustls-platform-verifier: {err:?}");
            jni::sys::JNI_FALSE
        }
    }
}

// ---------------------------------------------------------------------------
// FFI-safe types
// ---------------------------------------------------------------------------

#[derive(uniffi::Error, Debug, thiserror::Error)]
pub enum PaykitFfiError {
    #[error("Transport error: {reason}")]
    Transport { reason: String },
    #[error("Not found: {reason}")]
    NotFound { reason: String },
    #[error("Invalid data: {reason}")]
    InvalidData { reason: String },
    #[error("Validation error: {reason}")]
    Validation { reason: String },
    #[error("Session error: {reason}")]
    Session { reason: String },
}

impl From<paykit_lib::PaykitError> for PaykitFfiError {
    fn from(err: paykit_lib::PaykitError) -> Self {
        match err {
            paykit_lib::PaykitError::Transport { context, source } => PaykitFfiError::Transport {
                reason: format!("{context}: {source}"),
            },
            paykit_lib::PaykitError::NotFound(msg) => PaykitFfiError::NotFound { reason: msg },
            paykit_lib::PaykitError::InvalidData { context, source } => {
                let detail = source.map(|s| format!("{context}: {s}")).unwrap_or(context);
                PaykitFfiError::InvalidData { reason: detail }
            }
            paykit_lib::PaykitError::Validation(msg) => PaykitFfiError::Validation { reason: msg },
        }
    }
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiPaymentEntry {
    pub method_id: String,
    pub endpoint_data: String,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiPrivatePaymentsPayload {
    pub reference: String,
    pub entries: Vec<FfiPaymentEntry>,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiHandshakeProgress {
    pub status: String,
    pub handle_id: String,
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static RUNTIME: OnceCell<Runtime> = OnceCell::new();
static PUBKY: OnceCell<Pubky> = OnceCell::new();

struct SessionState {
    transport: PubkyAuthenticatedTransport,
    session: PubkySession,
}

enum StoredHandshakeState {
    Live(Box<EncryptedLinkHandshake>),
    Snapshot(Vec<u8>),
}

struct StoredHandshake {
    secret_key: [u8; 32],
    max_recovery_attempts: u32,
    state: StoredHandshakeState,
}

static SESSION: OnceCell<TokioMutex<Option<SessionState>>> = OnceCell::new();
static HANDSHAKES: OnceCell<TokioMutex<HashMap<u64, StoredHandshake>>> = OnceCell::new();
type LinkHandle = Arc<TokioMutex<Option<EncryptedLink>>>;
static LINKS: OnceCell<TokioMutex<HashMap<u64, LinkHandle>>> = OnceCell::new();
static NEXT_HANDLE_ID: AtomicU64 = AtomicU64::new(1);

fn ensure_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| Runtime::new().expect("Failed to create Tokio runtime"))
}

fn get_session_lock() -> &'static TokioMutex<Option<SessionState>> {
    SESSION.get_or_init(|| TokioMutex::new(None))
}

fn get_handshake_lock() -> &'static TokioMutex<HashMap<u64, StoredHandshake>> {
    HANDSHAKES.get_or_init(|| TokioMutex::new(HashMap::new()))
}

fn get_link_lock() -> &'static TokioMutex<HashMap<u64, LinkHandle>> {
    LINKS.get_or_init(|| TokioMutex::new(HashMap::new()))
}

fn get_pubky_client() -> Result<&'static Pubky, PaykitFfiError> {
    PUBKY.get().ok_or_else(|| PaykitFfiError::Session {
        reason: "Paykit not initialized. Call paykit_initialize() first.".into(),
    })
}

fn parse_public_key(pk_str: &str) -> Result<PublicKey, PaykitFfiError> {
    pk_str
        .parse::<PublicKey>()
        .map_err(|e| PaykitFfiError::Validation {
            reason: format!("Invalid public key '{pk_str}': {e}"),
        })
}

fn make_reader(pubky: &Pubky) -> PubkyUnauthenticatedTransport {
    PubkyUnauthenticatedTransport::new(pubky.public_storage())
}

fn runtime_err(e: tokio::task::JoinError) -> PaykitFfiError {
    PaykitFfiError::Session {
        reason: format!("Runtime error: {e}"),
    }
}

fn next_handle_id() -> u64 {
    NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed)
}

fn handle_to_string(handle_id: u64) -> String {
    handle_id.to_string()
}

fn parse_handle_id(value: &str, label: &'static str) -> Result<u64, PaykitFfiError> {
    value
        .parse::<u64>()
        .map_err(|e| PaykitFfiError::Validation {
            reason: format!("Invalid {label} handle '{value}': {e}"),
        })
}

/// Clone the transport out of the session lock so network I/O doesn't hold it.
async fn get_authenticated_transport() -> Result<PubkyAuthenticatedTransport, PaykitFfiError> {
    let guard = get_session_lock().lock().await;
    let state = guard.as_ref().ok_or_else(|| PaykitFfiError::Session {
        reason: "No active session. Call paykit_import_session or paykit_sign_in first.".into(),
    })?;
    Ok(state.transport.clone())
}

/// Clone the session out of the lock so private-payment I/O doesn't hold it.
async fn get_session() -> Result<PubkySession, PaykitFfiError> {
    let guard = get_session_lock().lock().await;
    let state = guard.as_ref().ok_or_else(|| PaykitFfiError::Session {
        reason: "No active session. Call paykit_import_session or paykit_sign_in first.".into(),
    })?;
    Ok(state.session.clone())
}

fn parse_secret_key(hex_str: &str) -> Result<[u8; 32], PaykitFfiError> {
    let bytes = hex::decode(hex_str).map_err(|e| PaykitFfiError::Validation {
        reason: format!("Invalid hex secret key: {e}"),
    })?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| PaykitFfiError::Validation {
            reason: format!(
                "Secret key must be exactly 32 bytes (64 hex chars), got {} bytes",
                v.len()
            ),
        })
}

fn encode_snapshot(bytes: Vec<u8>) -> String {
    hex::encode(bytes)
}

fn decode_snapshot(encoded: &str, label: &'static str) -> Result<Vec<u8>, PaykitFfiError> {
    hex::decode(encoded).map_err(|e| PaykitFfiError::InvalidData {
        reason: format!("Invalid hex {label}: {e}"),
    })
}

fn entries_to_map(
    entries: Vec<FfiPaymentEntry>,
) -> Result<HashMap<MethodId, EndpointData>, PaykitFfiError> {
    entries
        .into_iter()
        .map(|entry| {
            Ok((
                MethodId::new(entry.method_id)?,
                EndpointData::new(entry.endpoint_data),
            ))
        })
        .collect()
}

fn map_to_entries(payments: paykit_lib::SupportedPayments) -> Vec<FfiPaymentEntry> {
    entries_map_to_entries(payments.entries)
}

fn entries_map_to_entries(entries: HashMap<MethodId, EndpointData>) -> Vec<FfiPaymentEntry> {
    entries
        .into_iter()
        .map(|(method, data)| FfiPaymentEntry {
            method_id: method.as_str().to_string(),
            endpoint_data: data.into_inner(),
        })
        .collect()
}

fn private_payload_to_lib(
    payload: FfiPrivatePaymentsPayload,
) -> Result<PrivatePaymentsPayload, PaykitFfiError> {
    Ok(PrivatePaymentsPayload::new(
        PaymentReference::new(payload.reference)?,
        entries_to_map(payload.entries)?,
    ))
}

fn private_payload_to_ffi(payload: PrivatePaymentsPayload) -> FfiPrivatePaymentsPayload {
    FfiPrivatePaymentsPayload {
        reference: payload.reference.as_str().to_string(),
        entries: entries_map_to_entries(payload.entries),
    }
}

async fn get_link_handle(link_id: u64) -> Result<LinkHandle, PaykitFfiError> {
    get_link_lock()
        .lock()
        .await
        .get(&link_id)
        .cloned()
        .ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Unknown encrypted-link handle: {link_id}"),
        })
}

async fn insert_handshake_handle(handshake_id: u64, handshake: StoredHandshake) {
    get_handshake_lock()
        .lock()
        .await
        .insert(handshake_id, handshake);
}

async fn restore_stored_handshake(
    secret_key: [u8; 32],
    snapshot_bytes: &[u8],
    max_recovery_attempts: u32,
) -> Result<EncryptedLinkHandshake, PaykitFfiError> {
    let snapshot = EncryptedLinkHandshakeSnapshot::deserialize(snapshot_bytes)?;
    let remote_pubkey = snapshot.recipient().clone();
    let session = get_session().await?;
    let pubky = get_pubky_client()?.clone();

    let mut handshake = paykit_lib::restore_encrypted_link_handshake(
        session,
        secret_key,
        &remote_pubkey,
        pubky,
        snapshot,
    )
    .await?;
    handshake.set_max_recovery_attempts(max_recovery_attempts);
    Ok(handshake)
}

async fn clear_private_handles() {
    get_handshake_lock().lock().await.clear();

    let mut guard = get_link_lock().lock().await;
    let links = std::mem::take(&mut *guard);
    drop(guard);

    for handle in links.into_values() {
        if let Some(link) = handle.lock().await.take() {
            let _ = paykit_lib::close_encrypted_link(link).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Create the Pubky SDK facade and initialize logging. Call once at app startup.
///
/// Targets the **production** network.
///
/// Safe to call multiple times — subsequent calls are no-ops if the first
/// succeeded. If it fails (e.g. network issue), call it again to retry.
#[uniffi::export]
pub async fn paykit_initialize() -> Result<(), PaykitFfiError> {
    #[cfg(target_os = "android")]
    init_android_logger();

    let rt = ensure_runtime();
    rt.spawn(async {
        PUBKY.get_or_try_init(|| {
            Pubky::new().map_err(|e| PaykitFfiError::Session {
                reason: format!("Failed to initialize Pubky SDK: {e}"),
            })
        })?;
        let _ = get_session_lock();
        let _ = get_handshake_lock();
        let _ = get_link_lock();
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

// ---------------------------------------------------------------------------
// Session queries
// ---------------------------------------------------------------------------

/// Returns `true` if an authenticated session is currently active.
#[uniffi::export]
pub async fn paykit_is_authenticated() -> bool {
    let rt = ensure_runtime();
    rt.spawn(async {
        let guard = get_session_lock().lock().await;
        guard.is_some()
    })
    .await
    .unwrap_or(false)
}

/// Returns the public key of the currently authenticated user, or `None`.
#[uniffi::export]
pub async fn paykit_get_current_public_key() -> Option<String> {
    let rt = ensure_runtime();
    rt.spawn(async {
        let guard = get_session_lock().lock().await;
        guard
            .as_ref()
            .map(|s| s.session.info().public_key().to_string())
    })
    .await
    .unwrap_or(None)
}

/// Exports the current session secret for persistence across app restarts.
///
/// Returns the compact `<pubkey_z32>:<cookie_secret>` string that can be
/// passed back to `paykit_import_session` on next cold start.
#[uniffi::export]
pub async fn paykit_export_session() -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async {
        let guard = get_session_lock().lock().await;
        let state = guard.as_ref().ok_or_else(|| PaykitFfiError::Session {
            reason: "No active session to export.".into(),
        })?;
        Ok(state.session.export_secret())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

// ---------------------------------------------------------------------------
// Read operations
// ---------------------------------------------------------------------------

/// Fetch all published payment methods for a user.
#[uniffi::export]
pub async fn paykit_get_payment_list(
    public_key: String,
) -> Result<Vec<FfiPaymentEntry>, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let pubky = get_pubky_client()?;
        let pk = parse_public_key(&public_key)?;
        let reader = make_reader(pubky);
        let payments = paykit_lib::get_payment_list(&reader, &pk).await?;
        Ok(map_to_entries(payments))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Fetch a single payment endpoint for a user and method. Returns `None` if not set.
#[uniffi::export]
pub async fn paykit_get_payment_endpoint(
    public_key: String,
    method_id: String,
) -> Result<Option<String>, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let pubky = get_pubky_client()?;
        let pk = parse_public_key(&public_key)?;
        let method = MethodId::new(method_id)?;
        let reader = make_reader(pubky);
        let endpoint = paykit_lib::get_payment_endpoint(&reader, &pk, &method).await?;
        Ok(endpoint.map(|d| d.into_inner()))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// Import a session from a Pubky Ring auth flow.
///
/// Accepts a compact session secret (`<pubkey_z32>:<cookie_secret>`) produced
/// by `PubkySession::export_secret()`. Validates with the homeserver and stores
/// the session for subsequent write operations.
#[uniffi::export]
pub async fn paykit_import_session(session_secret: String) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let pubky = get_pubky_client()?;
        let client = pubky.client().clone();
        let session = PubkySession::import_secret(&session_secret, Some(client))
            .await
            .map_err(|e| PaykitFfiError::Session {
                reason: format!("Failed to import session: {e}"),
            })?;

        let public_key = session.info().public_key().to_string();
        let transport = PubkyAuthenticatedTransport::new(session.clone());

        clear_private_handles().await;

        let mut guard = get_session_lock().lock().await;
        *guard = Some(SessionState { transport, session });

        Ok(public_key)
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Sign up for a new account using a raw secret key. Only available with
/// the `dev-auth` feature (enabled by default, disable for production builds).
#[cfg(feature = "dev-auth")]
#[uniffi::export]
pub async fn paykit_sign_up(
    secret_key_hex: String,
    homeserver_public_key: String,
) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let pubky = get_pubky_client()?;
        let keypair = keypair_from_hex(&secret_key_hex)?;
        let hs_pk = parse_public_key(&homeserver_public_key)?;

        let signer = pubky.signer(keypair);
        let session = signer
            .signup(&hs_pk, None)
            .await
            .map_err(|e| PaykitFfiError::Session {
                reason: format!("Signup failed: {e}"),
            })?;

        let public_key = session.info().public_key().to_string();
        let transport = PubkyAuthenticatedTransport::new(session.clone());

        clear_private_handles().await;

        let mut guard = get_session_lock().lock().await;
        *guard = Some(SessionState { transport, session });

        Ok(public_key)
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Sign in with a raw secret key. Only available with the `dev-auth`
/// feature (enabled by default, disable for production builds).
///
/// The homeserver is resolved automatically via PKDNS.
#[cfg(feature = "dev-auth")]
#[uniffi::export]
pub async fn paykit_sign_in(secret_key_hex: String) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let pubky = get_pubky_client()?;
        let keypair = keypair_from_hex(&secret_key_hex)?;

        let signer = pubky.signer(keypair);
        let session = signer.signin().await.map_err(|e| PaykitFfiError::Session {
            reason: format!("Signin failed: {e}"),
        })?;

        let public_key = session.info().public_key().to_string();
        let transport = PubkyAuthenticatedTransport::new(session.clone());

        clear_private_handles().await;

        let mut guard = get_session_lock().lock().await;
        *guard = Some(SessionState { transport, session });

        Ok(public_key)
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

// ---------------------------------------------------------------------------
// Write operations
// ---------------------------------------------------------------------------

/// Publish or update a payment endpoint for the authenticated user.
#[uniffi::export]
pub async fn paykit_set_payment_endpoint(
    method_id: String,
    endpoint_data: String,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let method = MethodId::new(method_id)?;
        let data = EndpointData::new(endpoint_data);
        let transport = get_authenticated_transport().await?;

        paykit_lib::set_payment_endpoint(&transport, method, data).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Remove a payment endpoint for the authenticated user.
#[uniffi::export]
pub async fn paykit_remove_payment_endpoint(method_id: String) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let method = MethodId::new(method_id)?;
        let transport = get_authenticated_transport().await?;

        paykit_lib::remove_payment_endpoint(&transport, method).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

// ---------------------------------------------------------------------------
// Private encrypted payments
// ---------------------------------------------------------------------------

/// Default maximum number of automatic private-payment send retries.
#[uniffi::export]
pub fn paykit_default_max_send_retries() -> u32 {
    paykit_lib::DEFAULT_MAX_SEND_RETRIES
}

/// Default maximum number of consecutive handshake recovery attempts.
#[uniffi::export]
pub fn paykit_default_max_recovery_attempts() -> u32 {
    paykit_lib::DEFAULT_MAX_RECOVERY_ATTEMPTS
}

/// Start a private-payment encrypted link as the initiator.
#[uniffi::export]
pub async fn paykit_initiate_encrypted_link(
    secret_key_hex: String,
    receiver_public_key: String,
) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let secret_key = parse_secret_key(&secret_key_hex)?;
        let receiver = parse_public_key(&receiver_public_key)?;
        let session = get_session().await?;
        let pubky = get_pubky_client()?.clone();

        let handshake = paykit_lib::initiate_encrypted_link(session, secret_key, &receiver, pubky)?;
        let handle_id = next_handle_id();
        insert_handshake_handle(
            handle_id,
            StoredHandshake {
                secret_key,
                max_recovery_attempts: paykit_lib::DEFAULT_MAX_RECOVERY_ATTEMPTS,
                state: StoredHandshakeState::Live(Box::new(handshake)),
            },
        )
        .await;
        Ok(handle_to_string(handle_id))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Start a private-payment encrypted link as the responder.
#[uniffi::export]
pub async fn paykit_accept_encrypted_link(
    secret_key_hex: String,
    sender_public_key: String,
) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let secret_key = parse_secret_key(&secret_key_hex)?;
        let sender = parse_public_key(&sender_public_key)?;
        let session = get_session().await?;
        let pubky = get_pubky_client()?.clone();

        let handshake = paykit_lib::accept_encrypted_link(session, secret_key, &sender, pubky)?;
        let handle_id = next_handle_id();
        insert_handshake_handle(
            handle_id,
            StoredHandshake {
                secret_key,
                max_recovery_attempts: paykit_lib::DEFAULT_MAX_RECOVERY_ATTEMPTS,
                state: StoredHandshakeState::Live(Box::new(handshake)),
            },
        )
        .await;
        Ok(handle_to_string(handle_id))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Advance an encrypted-link handshake by one polling-safe step.
///
/// Returns status `"pending"` with the same handshake handle, or `"complete"`
/// with a new encrypted-link handle.
#[uniffi::export]
pub async fn paykit_advance_handshake(
    handshake_id: String,
) -> Result<FfiHandshakeProgress, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let handshake_id = parse_handle_id(&handshake_id, "handshake")?;
        let stored = get_handshake_lock()
            .lock()
            .await
            .remove(&handshake_id)
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: format!("Unknown encrypted-link handshake handle: {handshake_id}"),
            })?;
        let StoredHandshake {
            secret_key,
            max_recovery_attempts,
            state,
        } = stored;

        let (mut handshake, pre_advance_snapshot) = match state {
            StoredHandshakeState::Live(handshake) => {
                let snapshot = handshake.serialize();
                (*handshake, snapshot)
            }
            StoredHandshakeState::Snapshot(snapshot) => {
                match restore_stored_handshake(secret_key, &snapshot, max_recovery_attempts).await {
                    Ok(handshake) => (handshake, snapshot),
                    Err(err) => {
                        insert_handshake_handle(
                            handshake_id,
                            StoredHandshake {
                                secret_key,
                                max_recovery_attempts,
                                state: StoredHandshakeState::Snapshot(snapshot),
                            },
                        )
                        .await;
                        return Err(err);
                    }
                }
            }
        };
        handshake.set_max_recovery_attempts(max_recovery_attempts);

        match paykit_lib::advance_handshake(handshake).await {
            Ok(HandshakeProgress::Pending(handshake)) => {
                insert_handshake_handle(
                    handshake_id,
                    StoredHandshake {
                        secret_key,
                        max_recovery_attempts,
                        state: StoredHandshakeState::Live(Box::new(handshake)),
                    },
                )
                .await;
                Ok(FfiHandshakeProgress {
                    status: "pending".into(),
                    handle_id: handle_to_string(handshake_id),
                })
            }
            Ok(HandshakeProgress::Complete(link)) => {
                let link_id = next_handle_id();
                get_link_lock()
                    .lock()
                    .await
                    .insert(link_id, Arc::new(TokioMutex::new(Some(link))));
                Ok(FfiHandshakeProgress {
                    status: "complete".into(),
                    handle_id: handle_to_string(link_id),
                })
            }
            Err(err) => {
                insert_handshake_handle(
                    handshake_id,
                    StoredHandshake {
                        secret_key,
                        max_recovery_attempts,
                        state: StoredHandshakeState::Snapshot(pre_advance_snapshot),
                    },
                )
                .await;
                Err(err.into())
            }
        }
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Configure automatic recovery attempts for a pending encrypted-link handshake.
#[uniffi::export]
pub async fn paykit_set_encrypted_link_handshake_max_recovery_attempts(
    handshake_id: String,
    max: u32,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let handshake_id = parse_handle_id(&handshake_id, "handshake")?;
        let mut guard = get_handshake_lock().lock().await;
        let handshake = guard
            .get_mut(&handshake_id)
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: format!("Unknown encrypted-link handshake handle: {handshake_id}"),
            })?;
        handshake.max_recovery_attempts = max;
        if let StoredHandshakeState::Live(handshake) = &mut handshake.state {
            handshake.set_max_recovery_attempts(max);
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Configure automatic send retries for an established encrypted link.
#[uniffi::export]
pub async fn paykit_set_encrypted_link_max_send_retries(
    link_id: String,
    max: u32,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted-link handle is closed: {link_id}"),
        })?;
        link.set_max_send_retries(max);
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Encrypt and send the complete private payments map over an established link.
#[uniffi::export]
pub async fn paykit_set_private_payments(
    link_id: String,
    payload: FfiPrivatePaymentsPayload,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let payload = private_payload_to_lib(payload)?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted-link handle is closed: {link_id}"),
        })?;
        paykit_lib::set_private_payments(link, &payload).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Receive and decrypt the latest private payments map from an established link.
#[uniffi::export]
pub async fn paykit_get_private_payments(
    link_id: String,
) -> Result<Option<FfiPrivatePaymentsPayload>, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted-link handle is closed: {link_id}"),
        })?;
        let payments = paykit_lib::get_private_payments(link).await?;
        Ok(payments.map(private_payload_to_ffi))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Serialize an in-progress handshake snapshot for durable storage.
#[uniffi::export]
pub async fn paykit_serialize_encrypted_link_handshake(
    handshake_id: String,
) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let handshake_id = parse_handle_id(&handshake_id, "handshake")?;
        let guard = get_handshake_lock().lock().await;
        let handshake = guard
            .get(&handshake_id)
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: format!("Unknown encrypted-link handshake handle: {handshake_id}"),
            })?;
        let snapshot = match &handshake.state {
            StoredHandshakeState::Live(handshake) => handshake.serialize(),
            StoredHandshakeState::Snapshot(snapshot) => snapshot.clone(),
        };
        Ok(encode_snapshot(snapshot))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Serialize an established encrypted link snapshot for durable storage.
#[uniffi::export]
pub async fn paykit_serialize_encrypted_link(link_id: String) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let handle = get_link_handle(link_id).await?;
        let guard = handle.lock().await;
        let link = guard.as_ref().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted-link handle is closed: {link_id}"),
        })?;
        Ok(encode_snapshot(link.serialize()))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Return the remote peer embedded in an encrypted-link snapshot.
#[uniffi::export]
pub fn paykit_encrypted_link_snapshot_recipient(
    snapshot_hex: String,
) -> Result<String, PaykitFfiError> {
    let snapshot_bytes = decode_snapshot(&snapshot_hex, "encrypted-link snapshot")?;
    let snapshot = EncryptedLinkSnapshot::deserialize(&snapshot_bytes)?;
    Ok(snapshot.recipient().to_string())
}

/// Return the remote peer embedded in a handshake snapshot.
#[uniffi::export]
pub fn paykit_encrypted_link_handshake_snapshot_recipient(
    snapshot_hex: String,
) -> Result<String, PaykitFfiError> {
    let snapshot_bytes = decode_snapshot(&snapshot_hex, "handshake snapshot")?;
    let snapshot = EncryptedLinkHandshakeSnapshot::deserialize(&snapshot_bytes)?;
    Ok(snapshot.recipient().to_string())
}

/// Restore an established encrypted link from a serialized snapshot.
#[uniffi::export]
pub async fn paykit_restore_encrypted_link(
    secret_key_hex: String,
    snapshot_hex: String,
) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let secret_key = parse_secret_key(&secret_key_hex)?;
        let snapshot_bytes = decode_snapshot(&snapshot_hex, "encrypted-link snapshot")?;
        let snapshot = EncryptedLinkSnapshot::deserialize(&snapshot_bytes)?;
        let remote_pubkey = snapshot.recipient().clone();
        let session = get_session().await?;
        let pubky = get_pubky_client()?.clone();

        let link = paykit_lib::restore_encrypted_link(
            session,
            secret_key,
            &remote_pubkey,
            pubky,
            snapshot,
        )
        .await?;
        let link_id = next_handle_id();
        get_link_lock()
            .lock()
            .await
            .insert(link_id, Arc::new(TokioMutex::new(Some(link))));
        Ok(handle_to_string(link_id))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Restore an in-progress encrypted-link handshake from a serialized snapshot.
#[uniffi::export]
pub async fn paykit_restore_encrypted_link_handshake(
    secret_key_hex: String,
    snapshot_hex: String,
) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let secret_key = parse_secret_key(&secret_key_hex)?;
        let snapshot_bytes = decode_snapshot(&snapshot_hex, "handshake snapshot")?;
        let snapshot = EncryptedLinkHandshakeSnapshot::deserialize(&snapshot_bytes)?;
        let remote_pubkey = snapshot.recipient().clone();
        let session = get_session().await?;
        let pubky = get_pubky_client()?.clone();

        let handshake = paykit_lib::restore_encrypted_link_handshake(
            session,
            secret_key,
            &remote_pubkey,
            pubky,
            snapshot,
        )
        .await?;
        let handshake_id = next_handle_id();
        insert_handshake_handle(
            handshake_id,
            StoredHandshake {
                secret_key,
                max_recovery_attempts: paykit_lib::DEFAULT_MAX_RECOVERY_ATTEMPTS,
                state: StoredHandshakeState::Live(Box::new(handshake)),
            },
        )
        .await;
        Ok(handle_to_string(handshake_id))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Close an established encrypted link and remove its FFI handle.
#[uniffi::export]
pub async fn paykit_close_encrypted_link(link_id: String) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let handle = get_link_lock()
            .lock()
            .await
            .remove(&link_id)
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: format!("Unknown encrypted-link handle: {link_id}"),
            })?;
        let link = handle
            .lock()
            .await
            .take()
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: format!("Encrypted-link handle is closed: {link_id}"),
            })?;
        paykit_lib::close_encrypted_link(link).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Drop an in-progress encrypted-link handshake handle.
#[uniffi::export]
pub async fn paykit_drop_encrypted_link_handshake(
    handshake_id: String,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let handshake_id = parse_handle_id(&handshake_id, "handshake")?;
        get_handshake_lock()
            .lock()
            .await
            .remove(&handshake_id)
            .map(|_| ())
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: format!("Unknown encrypted-link handshake handle: {handshake_id}"),
            })
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// End the current session on the homeserver and clear local state.
///
/// If the server request fails the session is restored so no data is lost.
#[uniffi::export]
pub async fn paykit_sign_out() -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let mut guard = get_session_lock().lock().await;
        let state = guard.take().ok_or_else(|| PaykitFfiError::Session {
            reason: "No active session to sign out of.".into(),
        })?;

        match state.session.signout().await {
            Ok(()) => {
                drop(guard);
                clear_private_handles().await;
                Ok(())
            }
            Err((e, returned_session)) => {
                *guard = Some(SessionState {
                    transport: PubkyAuthenticatedTransport::new(returned_session.clone()),
                    session: returned_session,
                });
                Err(PaykitFfiError::Session {
                    reason: format!("Signout failed: {e}"),
                })
            }
        }
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Discard the local session without contacting the homeserver.
///
/// Idempotent — safe to call even when no session exists.
/// The server-side session will expire on its own.
#[uniffi::export]
pub async fn paykit_force_sign_out() {
    let rt = ensure_runtime();
    let _ = rt
        .spawn(async move {
            let mut guard = get_session_lock().lock().await;
            guard.take();
            drop(guard);
            clear_private_handles().await;
        })
        .await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(feature = "dev-auth")]
fn keypair_from_hex(hex_str: &str) -> Result<Keypair, PaykitFfiError> {
    let secret = parse_secret_key(hex_str)?;
    Ok(Keypair::from_secret(&secret))
}
