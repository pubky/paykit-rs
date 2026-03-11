uniffi::setup_scaffolding!();

use once_cell::sync::OnceCell;
#[cfg(feature = "dev-auth")]
use pubky::Keypair;
use pubky::{Pubky, PubkySession, PublicKey};
use tokio::runtime::Runtime;
use tokio::sync::Mutex as TokioMutex;

use paykit_lib::{
    EndpointData, MethodId, PubkyAuthenticatedTransport, PubkyUnauthenticatedTransport,
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

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static RUNTIME: OnceCell<Runtime> = OnceCell::new();
static PUBKY: OnceCell<Pubky> = OnceCell::new();

struct SessionState {
    transport: PubkyAuthenticatedTransport,
    session: PubkySession,
}

static SESSION: OnceCell<TokioMutex<Option<SessionState>>> = OnceCell::new();

fn ensure_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| Runtime::new().expect("Failed to create Tokio runtime"))
}

fn get_session_lock() -> &'static TokioMutex<Option<SessionState>> {
    SESSION.get_or_init(|| TokioMutex::new(None))
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

/// Clone the transport out of the session lock so network I/O doesn't hold it.
async fn get_authenticated_transport() -> Result<PubkyAuthenticatedTransport, PaykitFfiError> {
    let guard = get_session_lock().lock().await;
    let state = guard.as_ref().ok_or_else(|| PaykitFfiError::Session {
        reason: "No active session. Call paykit_import_session or paykit_sign_in first.".into(),
    })?;
    Ok(state.transport.clone())
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
        Ok(payments
            .entries
            .into_iter()
            .map(|(method, data)| FfiPaymentEntry {
                method_id: method.as_str().to_string(),
                endpoint_data: data.into_inner(),
            })
            .collect())
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
            Ok(()) => Ok(()),
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
        })
        .await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(feature = "dev-auth")]
fn keypair_from_hex(hex_str: &str) -> Result<Keypair, PaykitFfiError> {
    let bytes = hex::decode(hex_str).map_err(|e| PaykitFfiError::Validation {
        reason: format!("Invalid hex secret key: {e}"),
    })?;
    let secret: [u8; 32] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| PaykitFfiError::Validation {
            reason: format!(
                "Secret key must be exactly 32 bytes (64 hex chars), got {} bytes",
                v.len()
            ),
        })?;
    Ok(Keypair::from_secret(&secret))
}
