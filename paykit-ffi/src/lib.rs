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
    BillingPeriod, EncryptedLink, EncryptedLinkHandshake, EncryptedLinkHandshakeSnapshot,
    EncryptedLinkSnapshot, EventId, HandshakeProgress, PaymentAmount, PaymentEndpointIdentifier,
    PaymentEndpointPayload, PaymentProof, PaymentReference, PaymentRequest,
    PaymentRequestAcceptance, PaymentRequestCancellation, PaymentRequestEvent,
    PaymentRequestEventMessage, PaymentRequestId, PaymentRequestRejection, PaymentRequestTerms,
    PreparedReceipt, PrivateApplicationMessage, PrivateMessageKind, PrivatePaymentList, Receipt,
    ReceiptAccess, ReceiptAccessEventMessage, ReceiptDecryptionKey, ReceiptDraft, ReceiptId,
    Recurrence, RecurrenceUnit,
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
pub struct FfiPaymentEndpoint {
    pub payment_endpoint_identifier: String,
    pub payment_endpoint_payload: String,
}

#[derive(uniffi::Record, Clone)]
pub struct FfiPrivatePaymentList {
    pub payment_endpoints: Vec<FfiPaymentEndpoint>,
}

impl std::fmt::Debug for FfiPrivatePaymentList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPrivatePaymentList")
            .field(
                "payment_endpoints",
                &format_args!("<redacted:{} endpoints>", self.payment_endpoints.len()),
            )
            .finish()
    }
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiHandshakeProgress {
    pub status: String,
    pub handle_id: String,
}

#[derive(uniffi::Record, Clone)]
pub struct FfiPrivateApplicationMessage {
    pub version: Option<u32>,
    pub kind: Option<String>,
    pub raw_json: String,
}

impl std::fmt::Debug for FfiPrivateApplicationMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPrivateApplicationMessage")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field(
                "raw_json",
                &format_args!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .finish()
    }
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiPaymentAmount {
    pub value: String,
    pub asset: String,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiBillingPeriod {
    pub starts_at: String,
    pub ends_at: String,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiRecurrence {
    pub every: u32,
    pub unit: String,
    pub starts_at: String,
    pub anchor: String,
    pub ends_at: Option<String>,
}

#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentRequestTerms {
    pub amount: FfiPaymentAmount,
    pub payment_reference: String,
    pub proposal_expires_at: Option<String>,
    pub recurrence: Option<FfiRecurrence>,
    pub accepted_payment_endpoint_identifiers: Vec<String>,
    pub metadata_json: String,
}

impl std::fmt::Debug for FfiPaymentRequestTerms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPaymentRequestTerms")
            .field("amount", &"<redacted>")
            .field("payment_reference", &self.payment_reference)
            .field("proposal_expires_at", &self.proposal_expires_at)
            .field("recurrence", &self.recurrence)
            .field(
                "accepted_payment_endpoint_identifiers",
                &self.accepted_payment_endpoint_identifiers,
            )
            .field(
                "metadata_json",
                &format_args!("<redacted:{} bytes>", self.metadata_json.len()),
            )
            .finish()
    }
}

#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentRequest {
    pub event_id: String,
    pub payment_request_id: String,
    pub request: FfiPaymentRequestTerms,
}

impl std::fmt::Debug for FfiPaymentRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPaymentRequest")
            .field("event_id", &self.event_id)
            .field("payment_request_id", &self.payment_request_id)
            .field("request", &self.request)
            .finish()
    }
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiPaymentRequestAcceptance {
    pub event_id: String,
    pub payment_request_id: String,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiPaymentRequestRejection {
    pub event_id: String,
    pub payment_request_id: String,
    pub reason: Option<String>,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiPaymentRequestCancellation {
    pub event_id: String,
    pub payment_request_id: String,
    pub reason: Option<String>,
}

#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentProof {
    pub event_id: String,
    pub payment_request_id: String,
    pub payment_reference: String,
    pub billing_period: Option<FfiBillingPeriod>,
    pub payment_endpoint_identifier: String,
    pub proof_json: String,
}

impl std::fmt::Debug for FfiPaymentProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPaymentProof")
            .field("event_id", &self.event_id)
            .field("payment_request_id", &self.payment_request_id)
            .field("payment_reference", &self.payment_reference)
            .field("billing_period", &self.billing_period)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field(
                "proof_json",
                &format_args!("<redacted:{} bytes>", self.proof_json.len()),
            )
            .finish()
    }
}

#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentRequestEvent {
    pub event_type: String,
    pub request: Option<FfiPaymentRequest>,
    pub acceptance: Option<FfiPaymentRequestAcceptance>,
    pub rejection: Option<FfiPaymentRequestRejection>,
    pub cancellation: Option<FfiPaymentRequestCancellation>,
    pub proof: Option<FfiPaymentProof>,
}

impl std::fmt::Debug for FfiPaymentRequestEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPaymentRequestEvent")
            .field("event_type", &self.event_type)
            .field("request", &self.request)
            .field("acceptance", &self.acceptance)
            .field("rejection", &self.rejection)
            .field("cancellation", &self.cancellation)
            .field("proof", &self.proof)
            .finish()
    }
}

#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentRequestEventMessage {
    pub kind: String,
    pub event_id: Option<String>,
    pub payment_request_id: Option<String>,
    pub raw_json: String,
    pub event: Option<FfiPaymentRequestEvent>,
    pub validation_error: Option<String>,
}

impl std::fmt::Debug for FfiPaymentRequestEventMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPaymentRequestEventMessage")
            .field("kind", &self.kind)
            .field("event_id", &self.event_id)
            .field("payment_request_id", &self.payment_request_id)
            .field(
                "raw_json",
                &format_args!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .field(
                "event_type",
                &self.event.as_ref().map(|event| &event.event_type),
            )
            .field("validation_error", &self.validation_error)
            .finish()
    }
}

#[derive(uniffi::Record, Clone)]
pub struct FfiReceiptDraft {
    pub receipt_id: Option<String>,
    pub payment_reference: String,
    pub payment_request_id: Option<String>,
    pub billing_period: Option<FfiBillingPeriod>,
    pub payment_endpoint_identifier: Option<String>,
    pub amount: Option<FfiPaymentAmount>,
    pub metadata_json: String,
}

impl std::fmt::Debug for FfiReceiptDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiReceiptDraft")
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &self.payment_reference)
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field("amount", &self.amount.as_ref().map(|_| "<redacted>"))
            .field(
                "metadata_json",
                &format_args!("<redacted:{} bytes>", self.metadata_json.len()),
            )
            .finish()
    }
}

#[derive(uniffi::Record, Clone)]
pub struct FfiReceipt {
    pub receipt_id: String,
    pub payment_reference: String,
    pub payment_request_id: Option<String>,
    pub billing_period: Option<FfiBillingPeriod>,
    pub recipient_public_key: String,
    pub payment_endpoint_identifier: Option<String>,
    pub amount: Option<FfiPaymentAmount>,
    pub metadata_json: String,
}

impl std::fmt::Debug for FfiReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiReceipt")
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &self.payment_reference)
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field("recipient_public_key", &self.recipient_public_key)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field("amount", &self.amount.as_ref().map(|_| "<redacted>"))
            .field(
                "metadata_json",
                &format_args!("<redacted:{} bytes>", self.metadata_json.len()),
            )
            .finish()
    }
}

#[derive(uniffi::Record, Clone)]
pub struct FfiReceiptAccess {
    pub event_id: String,
    pub receipt_id: String,
    pub payment_reference: String,
    pub payment_request_id: Option<String>,
    pub billing_period: Option<FfiBillingPeriod>,
    pub location: String,
    pub key: String,
}

impl std::fmt::Debug for FfiReceiptAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiReceiptAccess")
            .field("event_id", &self.event_id)
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &self.payment_reference)
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field("location", &self.location)
            .field("key", &"<redacted>")
            .finish()
    }
}

#[derive(uniffi::Record, Clone)]
pub struct FfiReceiptAccessEventMessage {
    pub kind: String,
    pub event_id: Option<String>,
    pub receipt_id: Option<String>,
    pub raw_json: String,
    pub access: Option<FfiReceiptAccess>,
    pub validation_error: Option<String>,
}

impl std::fmt::Debug for FfiReceiptAccessEventMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiReceiptAccessEventMessage")
            .field("kind", &self.kind)
            .field("event_id", &self.event_id)
            .field("receipt_id", &self.receipt_id)
            .field(
                "raw_json",
                &format_args!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .field("parsed", &self.access.is_some())
            .field("validation_error", &self.validation_error)
            .finish()
    }
}

#[derive(uniffi::Record, Clone)]
pub struct FfiPreparedReceipt {
    pub receipt: FfiReceipt,
    pub encrypted_receipt: String,
    pub access: FfiReceiptAccess,
}

impl std::fmt::Debug for FfiPreparedReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPreparedReceipt")
            .field("receipt", &self.receipt)
            .field(
                "encrypted_receipt",
                &format_args!("<redacted:{} bytes>", self.encrypted_receipt.len()),
            )
            .field("access", &self.access)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static RUNTIME: OnceCell<Runtime> = OnceCell::new();
static PUBKY: OnceCell<Pubky> = OnceCell::new();

struct SessionState {
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

fn make_reader(pubky: &Pubky) -> pubky::PublicStorage {
    pubky.public_storage()
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

/// Clone the session out of the lock so private Paykit message I/O doesn't hold it.
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

fn payment_endpoints_to_map(
    payment_endpoints: Vec<FfiPaymentEndpoint>,
) -> Result<HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>, PaykitFfiError> {
    let mut endpoints = HashMap::new();
    for payment_endpoint in payment_endpoints {
        let identifier =
            PaymentEndpointIdentifier::new(payment_endpoint.payment_endpoint_identifier)?;
        if endpoints
            .insert(
                identifier.clone(),
                PaymentEndpointPayload::new(payment_endpoint.payment_endpoint_payload),
            )
            .is_some()
        {
            return Err(PaykitFfiError::Validation {
                reason: format!(
                    "duplicate Payment Endpoint Identifier '{}'",
                    identifier.as_str()
                ),
            });
        }
    }

    Ok(endpoints)
}

fn payment_list_to_ffi(payments: paykit_lib::PaymentList) -> Vec<FfiPaymentEndpoint> {
    payment_endpoints_map_to_ffi(payments.payment_endpoints)
}

fn payment_endpoints_map_to_ffi(
    payment_endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
) -> Vec<FfiPaymentEndpoint> {
    payment_endpoints
        .into_iter()
        .map(|(identifier, payload)| FfiPaymentEndpoint {
            payment_endpoint_identifier: identifier.as_str().to_string(),
            payment_endpoint_payload: payload.into_inner(),
        })
        .collect()
}

fn private_payment_list_to_lib(
    list: FfiPrivatePaymentList,
) -> Result<PrivatePaymentList, PaykitFfiError> {
    Ok(PrivatePaymentList::new(payment_endpoints_to_map(
        list.payment_endpoints,
    )?))
}

fn receipt_metadata_to_json(
    metadata: serde_json::Map<String, serde_json::Value>,
) -> Result<String, PaykitFfiError> {
    json_object_to_string(metadata, "Receipt Metadata")
}

fn receipt_metadata_from_json(
    metadata_json: String,
) -> Result<serde_json::Map<String, serde_json::Value>, PaykitFfiError> {
    json_object_from_string(metadata_json, "Receipt Metadata")
}

fn json_object_to_string(
    object: serde_json::Map<String, serde_json::Value>,
    label: &'static str,
) -> Result<String, PaykitFfiError> {
    serde_json::to_string(&object).map_err(|err| PaykitFfiError::InvalidData {
        reason: format!("failed to serialize {label} JSON: {err}"),
    })
}

fn json_object_from_string(
    json: String,
    label: &'static str,
) -> Result<serde_json::Map<String, serde_json::Value>, PaykitFfiError> {
    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|err| PaykitFfiError::Validation {
            reason: format!("{label} must be a JSON object: {err}"),
        })?;
    match value {
        serde_json::Value::Object(object) => Ok(object),
        _ => Err(PaykitFfiError::Validation {
            reason: format!("{label} must be a JSON object"),
        }),
    }
}

fn payment_amount_to_ffi(amount: PaymentAmount) -> FfiPaymentAmount {
    FfiPaymentAmount {
        value: amount.value,
        asset: amount.asset,
    }
}

fn payment_amount_to_lib(amount: FfiPaymentAmount) -> Result<PaymentAmount, PaykitFfiError> {
    Ok(PaymentAmount::new(amount.value, amount.asset)?)
}

fn billing_period_to_ffi(period: BillingPeriod) -> FfiBillingPeriod {
    FfiBillingPeriod {
        starts_at: period.starts_at,
        ends_at: period.ends_at,
    }
}

fn billing_period_to_lib(period: FfiBillingPeriod) -> BillingPeriod {
    BillingPeriod {
        starts_at: period.starts_at,
        ends_at: period.ends_at,
    }
}

fn recurrence_unit_to_ffi(unit: RecurrenceUnit) -> String {
    match unit {
        RecurrenceUnit::Minute => "minute",
        RecurrenceUnit::Hour => "hour",
        RecurrenceUnit::Day => "day",
        RecurrenceUnit::Week => "week",
        RecurrenceUnit::Month => "month",
        RecurrenceUnit::Year => "year",
    }
    .to_string()
}

fn recurrence_unit_to_lib(unit: String) -> Result<RecurrenceUnit, PaykitFfiError> {
    match unit.as_str() {
        "minute" => Ok(RecurrenceUnit::Minute),
        "hour" => Ok(RecurrenceUnit::Hour),
        "day" => Ok(RecurrenceUnit::Day),
        "week" => Ok(RecurrenceUnit::Week),
        "month" => Ok(RecurrenceUnit::Month),
        "year" => Ok(RecurrenceUnit::Year),
        _ => Err(PaykitFfiError::Validation {
            reason: format!("unsupported Recurrence unit '{unit}'"),
        }),
    }
}

fn recurrence_to_ffi(recurrence: Recurrence) -> FfiRecurrence {
    FfiRecurrence {
        every: recurrence.every,
        unit: recurrence_unit_to_ffi(recurrence.unit),
        starts_at: recurrence.starts_at,
        anchor: recurrence.anchor,
        ends_at: recurrence.ends_at,
    }
}

fn recurrence_to_lib(recurrence: FfiRecurrence) -> Result<Recurrence, PaykitFfiError> {
    Ok(Recurrence {
        every: recurrence.every,
        unit: recurrence_unit_to_lib(recurrence.unit)?,
        starts_at: recurrence.starts_at,
        anchor: recurrence.anchor,
        ends_at: recurrence.ends_at,
    })
}

fn payment_request_terms_to_ffi(
    terms: PaymentRequestTerms,
) -> Result<FfiPaymentRequestTerms, PaykitFfiError> {
    Ok(FfiPaymentRequestTerms {
        amount: payment_amount_to_ffi(terms.amount),
        payment_reference: terms.payment_reference.as_str().to_string(),
        proposal_expires_at: terms.proposal_expires_at,
        recurrence: terms.recurrence.map(recurrence_to_ffi),
        accepted_payment_endpoint_identifiers: terms
            .accepted_payment_endpoint_identifiers
            .into_iter()
            .map(|identifier| identifier.as_str().to_string())
            .collect(),
        metadata_json: json_object_to_string(terms.metadata, "Payment Request metadata")?,
    })
}

fn payment_request_terms_to_lib(
    terms: FfiPaymentRequestTerms,
) -> Result<PaymentRequestTerms, PaykitFfiError> {
    Ok(PaymentRequestTerms {
        amount: payment_amount_to_lib(terms.amount)?,
        payment_reference: PaymentReference::new(terms.payment_reference)?,
        proposal_expires_at: terms.proposal_expires_at,
        recurrence: terms.recurrence.map(recurrence_to_lib).transpose()?,
        accepted_payment_endpoint_identifiers: terms
            .accepted_payment_endpoint_identifiers
            .into_iter()
            .map(PaymentEndpointIdentifier::new)
            .collect::<paykit_lib::Result<Vec<_>>>()?,
        metadata: json_object_from_string(terms.metadata_json, "Payment Request metadata")?,
    })
}

fn payment_request_to_ffi(request: PaymentRequest) -> Result<FfiPaymentRequest, PaykitFfiError> {
    Ok(FfiPaymentRequest {
        event_id: request.event_id.as_str().to_string(),
        payment_request_id: request.payment_request_id.as_str().to_string(),
        request: payment_request_terms_to_ffi(request.request)?,
    })
}

fn payment_request_to_lib(request: FfiPaymentRequest) -> Result<PaymentRequest, PaykitFfiError> {
    Ok(PaymentRequest::new(
        EventId::new(request.event_id)?,
        PaymentRequestId::new(request.payment_request_id)?,
        payment_request_terms_to_lib(request.request)?,
    ))
}

fn payment_request_acceptance_to_ffi(
    event: PaymentRequestAcceptance,
) -> FfiPaymentRequestAcceptance {
    FfiPaymentRequestAcceptance {
        event_id: event.event_id.as_str().to_string(),
        payment_request_id: event.payment_request_id.as_str().to_string(),
    }
}

fn payment_request_acceptance_to_lib(
    event: FfiPaymentRequestAcceptance,
) -> Result<PaymentRequestAcceptance, PaykitFfiError> {
    Ok(PaymentRequestAcceptance::new(
        EventId::new(event.event_id)?,
        PaymentRequestId::new(event.payment_request_id)?,
    ))
}

fn payment_request_rejection_to_ffi(event: PaymentRequestRejection) -> FfiPaymentRequestRejection {
    FfiPaymentRequestRejection {
        event_id: event.event_id.as_str().to_string(),
        payment_request_id: event.payment_request_id.as_str().to_string(),
        reason: event.reason,
    }
}

fn payment_request_rejection_to_lib(
    event: FfiPaymentRequestRejection,
) -> Result<PaymentRequestRejection, PaykitFfiError> {
    Ok(PaymentRequestRejection::new(
        EventId::new(event.event_id)?,
        PaymentRequestId::new(event.payment_request_id)?,
        event.reason,
    ))
}

fn payment_request_cancellation_to_ffi(
    event: PaymentRequestCancellation,
) -> FfiPaymentRequestCancellation {
    FfiPaymentRequestCancellation {
        event_id: event.event_id.as_str().to_string(),
        payment_request_id: event.payment_request_id.as_str().to_string(),
        reason: event.reason,
    }
}

fn payment_request_cancellation_to_lib(
    event: FfiPaymentRequestCancellation,
) -> Result<PaymentRequestCancellation, PaykitFfiError> {
    Ok(PaymentRequestCancellation::new(
        EventId::new(event.event_id)?,
        PaymentRequestId::new(event.payment_request_id)?,
        event.reason,
    ))
}

fn payment_proof_to_ffi(proof: PaymentProof) -> Result<FfiPaymentProof, PaykitFfiError> {
    Ok(FfiPaymentProof {
        event_id: proof.event_id.as_str().to_string(),
        payment_request_id: proof.payment_request_id.as_str().to_string(),
        payment_reference: proof.payment_reference.as_str().to_string(),
        billing_period: proof.billing_period.map(billing_period_to_ffi),
        payment_endpoint_identifier: proof.payment_endpoint_identifier.as_str().to_string(),
        proof_json: json_object_to_string(proof.proof, "Payment Proof proof")?,
    })
}

fn payment_proof_to_lib(proof: FfiPaymentProof) -> Result<PaymentProof, PaykitFfiError> {
    Ok(PaymentProof::new(
        EventId::new(proof.event_id)?,
        PaymentRequestId::new(proof.payment_request_id)?,
        PaymentReference::new(proof.payment_reference)?,
        proof.billing_period.map(billing_period_to_lib),
        PaymentEndpointIdentifier::new(proof.payment_endpoint_identifier)?,
        json_object_from_string(proof.proof_json, "Payment Proof proof")?,
    ))
}

fn payment_request_event_to_ffi(
    event: PaymentRequestEvent,
) -> Result<FfiPaymentRequestEvent, PaykitFfiError> {
    match event {
        PaymentRequestEvent::Request(event) => Ok(FfiPaymentRequestEvent {
            event_type: "request".into(),
            request: Some(payment_request_to_ffi(event)?),
            acceptance: None,
            rejection: None,
            cancellation: None,
            proof: None,
        }),
        PaymentRequestEvent::Acceptance(event) => Ok(FfiPaymentRequestEvent {
            event_type: "acceptance".into(),
            request: None,
            acceptance: Some(payment_request_acceptance_to_ffi(event)),
            rejection: None,
            cancellation: None,
            proof: None,
        }),
        PaymentRequestEvent::Rejection(event) => Ok(FfiPaymentRequestEvent {
            event_type: "rejection".into(),
            request: None,
            acceptance: None,
            rejection: Some(payment_request_rejection_to_ffi(event)),
            cancellation: None,
            proof: None,
        }),
        PaymentRequestEvent::Cancellation(event) => Ok(FfiPaymentRequestEvent {
            event_type: "cancellation".into(),
            request: None,
            acceptance: None,
            rejection: None,
            cancellation: Some(payment_request_cancellation_to_ffi(event)),
            proof: None,
        }),
        PaymentRequestEvent::Proof(event) => Ok(FfiPaymentRequestEvent {
            event_type: "proof".into(),
            request: None,
            acceptance: None,
            rejection: None,
            cancellation: None,
            proof: Some(payment_proof_to_ffi(event)?),
        }),
    }
}

fn payment_request_event_to_lib(
    event: FfiPaymentRequestEvent,
) -> Result<PaymentRequestEvent, PaykitFfiError> {
    validate_payment_request_event_variant(&event)?;

    let event_type = event.event_type.clone();
    match event_type.as_str() {
        "request" => event
            .request
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: "Payment Request event_type 'request' requires request".into(),
            })
            .and_then(payment_request_to_lib)
            .map(PaymentRequestEvent::Request),
        "acceptance" => event
            .acceptance
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: "Payment Request event_type 'acceptance' requires acceptance".into(),
            })
            .and_then(payment_request_acceptance_to_lib)
            .map(PaymentRequestEvent::Acceptance),
        "rejection" => event
            .rejection
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: "Payment Request event_type 'rejection' requires rejection".into(),
            })
            .and_then(payment_request_rejection_to_lib)
            .map(PaymentRequestEvent::Rejection),
        "cancellation" => event
            .cancellation
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: "Payment Request event_type 'cancellation' requires cancellation".into(),
            })
            .and_then(payment_request_cancellation_to_lib)
            .map(PaymentRequestEvent::Cancellation),
        "proof" => event
            .proof
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: "Payment Request event_type 'proof' requires proof".into(),
            })
            .and_then(payment_proof_to_lib)
            .map(PaymentRequestEvent::Proof),
        _ => Err(PaykitFfiError::Validation {
            reason: format!("unsupported Payment Request event_type '{}'", event_type),
        }),
    }
}

fn validate_payment_request_event_variant(
    event: &FfiPaymentRequestEvent,
) -> Result<(), PaykitFfiError> {
    let populated = [
        ("request", event.request.is_some()),
        ("acceptance", event.acceptance.is_some()),
        ("rejection", event.rejection.is_some()),
        ("cancellation", event.cancellation.is_some()),
        ("proof", event.proof.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, is_present)| is_present.then_some(name))
    .collect::<Vec<_>>();

    if populated.len() != 1 {
        return Err(PaykitFfiError::Validation {
            reason: format!(
                "Payment Request event must include exactly one variant matching event_type '{}'; found {}",
                event.event_type,
                if populated.is_empty() {
                    "none".into()
                } else {
                    populated.join(", ")
                }
            ),
        });
    }

    let expected_type_is_known = matches!(
        event.event_type.as_str(),
        "request" | "acceptance" | "rejection" | "cancellation" | "proof"
    );
    if expected_type_is_known && populated[0] != event.event_type {
        return Err(PaykitFfiError::Validation {
            reason: format!(
                "Payment Request event variant '{}' must match event_type '{}'",
                populated[0], event.event_type
            ),
        });
    }

    Ok(())
}

fn payment_request_event_message_to_ffi(
    message: PaymentRequestEventMessage,
) -> Result<FfiPaymentRequestEventMessage, PaykitFfiError> {
    let (event, validation_error) = match message.event {
        Ok(event) => (Some(payment_request_event_to_ffi(event)?), None),
        Err(err) => (None, Some(err)),
    };
    Ok(FfiPaymentRequestEventMessage {
        kind: message.kind.as_str().to_string(),
        event_id: message.event_id.map(|id| id.as_str().to_string()),
        payment_request_id: message.payment_request_id.map(|id| id.as_str().to_string()),
        raw_json: message.raw_json,
        event,
        validation_error,
    })
}

fn private_application_message_to_lib(
    message: FfiPrivateApplicationMessage,
) -> Result<PrivateApplicationMessage, PaykitFfiError> {
    let version = message
        .version
        .map(|version| {
            u8::try_from(version).map_err(|_| PaykitFfiError::Validation {
                reason: format!(
                    "Private Application Message version must fit in u8, got {version}"
                ),
            })
        })
        .transpose()?;
    Ok(PrivateApplicationMessage {
        version,
        kind: message.kind,
        raw_json: message.raw_json,
    })
}

fn receipt_draft_to_lib(draft: FfiReceiptDraft) -> Result<ReceiptDraft, PaykitFfiError> {
    Ok(ReceiptDraft {
        receipt_id: draft.receipt_id.map(ReceiptId::new).transpose()?,
        payment_reference: PaymentReference::new(draft.payment_reference)?,
        payment_request_id: draft
            .payment_request_id
            .map(PaymentRequestId::new)
            .transpose()?,
        billing_period: draft.billing_period.map(billing_period_to_lib),
        payment_endpoint_identifier: draft
            .payment_endpoint_identifier
            .map(PaymentEndpointIdentifier::new)
            .transpose()?,
        amount: draft.amount.map(payment_amount_to_lib).transpose()?,
        metadata: receipt_metadata_from_json(draft.metadata_json)?,
    })
}

fn receipt_to_ffi(receipt: Receipt) -> Result<FfiReceipt, PaykitFfiError> {
    Ok(FfiReceipt {
        receipt_id: receipt.receipt_id.as_str().to_string(),
        payment_reference: receipt.payment_reference.as_str().to_string(),
        payment_request_id: receipt.payment_request_id.map(|id| id.as_str().to_string()),
        billing_period: receipt.billing_period.map(billing_period_to_ffi),
        recipient_public_key: receipt.recipient_public_key.to_string(),
        payment_endpoint_identifier: receipt
            .payment_endpoint_identifier
            .map(|identifier| identifier.as_str().to_string()),
        amount: receipt.amount.map(payment_amount_to_ffi),
        metadata_json: receipt_metadata_to_json(receipt.metadata)?,
    })
}

fn receipt_to_lib(receipt: FfiReceipt) -> Result<Receipt, PaykitFfiError> {
    Ok(Receipt {
        receipt_id: ReceiptId::new(receipt.receipt_id)?,
        payment_reference: PaymentReference::new(receipt.payment_reference)?,
        payment_request_id: receipt
            .payment_request_id
            .map(PaymentRequestId::new)
            .transpose()?,
        billing_period: receipt.billing_period.map(billing_period_to_lib),
        recipient_public_key: parse_public_key(&receipt.recipient_public_key)?,
        payment_endpoint_identifier: receipt
            .payment_endpoint_identifier
            .map(PaymentEndpointIdentifier::new)
            .transpose()?,
        amount: receipt.amount.map(payment_amount_to_lib).transpose()?,
        metadata: receipt_metadata_from_json(receipt.metadata_json)?,
    })
}

fn receipt_access_to_ffi(access: ReceiptAccess) -> FfiReceiptAccess {
    FfiReceiptAccess {
        event_id: access.event_id.as_str().to_string(),
        receipt_id: access.receipt_id.as_str().to_string(),
        payment_reference: access.payment_reference.as_str().to_string(),
        payment_request_id: access.payment_request_id.map(|id| id.as_str().to_string()),
        billing_period: access.billing_period.map(billing_period_to_ffi),
        location: access.location,
        key: access.key.as_str().to_string(),
    }
}

fn receipt_access_to_lib(access: FfiReceiptAccess) -> Result<ReceiptAccess, PaykitFfiError> {
    Ok(ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: EventId::new(access.event_id)?,
        receipt_id: ReceiptId::new(access.receipt_id)?,
        payment_reference: PaymentReference::new(access.payment_reference)?,
        payment_request_id: access
            .payment_request_id
            .map(PaymentRequestId::new)
            .transpose()?,
        billing_period: access.billing_period.map(billing_period_to_lib),
        location: access.location,
        key: ReceiptDecryptionKey::new(access.key)?,
    })
}

fn receipt_access_event_message_to_ffi(
    message: ReceiptAccessEventMessage,
) -> Result<FfiReceiptAccessEventMessage, PaykitFfiError> {
    let (access, validation_error) = match message.access {
        Ok(access) => (Some(receipt_access_to_ffi(access)), None),
        Err(err) => (None, Some(err)),
    };
    Ok(FfiReceiptAccessEventMessage {
        kind: message.kind.as_str().to_string(),
        event_id: message.event_id.map(|id| id.as_str().to_string()),
        receipt_id: message.receipt_id.map(|id| id.as_str().to_string()),
        raw_json: message.raw_json,
        access,
        validation_error,
    })
}

fn prepared_receipt_to_ffi(
    prepared: PreparedReceipt,
) -> Result<FfiPreparedReceipt, PaykitFfiError> {
    Ok(FfiPreparedReceipt {
        receipt: receipt_to_ffi(prepared.receipt)?,
        encrypted_receipt: prepared.encrypted_receipt,
        access: receipt_access_to_ffi(prepared.access),
    })
}

fn prepared_receipt_to_lib(
    prepared: FfiPreparedReceipt,
) -> Result<PreparedReceipt, PaykitFfiError> {
    Ok(PreparedReceipt {
        receipt: receipt_to_lib(prepared.receipt)?,
        encrypted_receipt: prepared.encrypted_receipt,
        access: receipt_access_to_lib(prepared.access)?,
    })
}

fn private_application_message_to_ffi(
    message: paykit_lib::PrivateApplicationMessage,
) -> FfiPrivateApplicationMessage {
    FfiPrivateApplicationMessage {
        version: message.version.map(u32::from),
        kind: message.kind,
        raw_json: message.raw_json,
    }
}

async fn get_link_handle(link_id: u64) -> Result<LinkHandle, PaykitFfiError> {
    get_link_lock()
        .lock()
        .await
        .get(&link_id)
        .cloned()
        .ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Unknown Encrypted Link handle: {link_id}"),
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

/// Fetch the public Payment List for a payee.
#[uniffi::export]
pub async fn paykit_get_payment_list(
    public_key: String,
) -> Result<Vec<FfiPaymentEndpoint>, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let pubky = get_pubky_client()?;
        let pk = parse_public_key(&public_key)?;
        let reader = make_reader(pubky);
        let payments = paykit_lib::get_payment_list(&reader, &pk).await?;
        Ok(payment_list_to_ffi(payments))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Fetch a single Payment Endpoint for a payee. Returns `None` if not set.
#[uniffi::export]
pub async fn paykit_get_payment_endpoint(
    public_key: String,
    payment_endpoint_identifier: String,
) -> Result<Option<String>, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let pubky = get_pubky_client()?;
        let pk = parse_public_key(&public_key)?;
        let identifier = PaymentEndpointIdentifier::new(payment_endpoint_identifier)?;
        let reader = make_reader(pubky);
        let endpoint = paykit_lib::get_payment_endpoint(&reader, &pk, &identifier).await?;
        Ok(endpoint.map(|payload| payload.into_inner()))
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
        clear_private_handles().await;

        let mut guard = get_session_lock().lock().await;
        *guard = Some(SessionState { session });

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
        clear_private_handles().await;

        let mut guard = get_session_lock().lock().await;
        *guard = Some(SessionState { session });

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
        clear_private_handles().await;

        let mut guard = get_session_lock().lock().await;
        *guard = Some(SessionState { session });

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
    payment_endpoint_identifier: String,
    payment_endpoint_payload: String,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let identifier = PaymentEndpointIdentifier::new(payment_endpoint_identifier)?;
        let payload = PaymentEndpointPayload::new(payment_endpoint_payload);
        let session = get_session().await?;

        paykit_lib::set_payment_endpoint(&session, identifier, payload).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Remove a payment endpoint for the authenticated user.
#[uniffi::export]
pub async fn paykit_remove_payment_endpoint(
    payment_endpoint_identifier: String,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let identifier = PaymentEndpointIdentifier::new(payment_endpoint_identifier)?;
        let session = get_session().await?;

        paykit_lib::remove_payment_endpoint(&session, identifier).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

// ---------------------------------------------------------------------------
// Private Payment Lists and Encrypted Links
// ---------------------------------------------------------------------------

/// Default maximum number of automatic Private Application Message send retries.
#[uniffi::export]
pub fn paykit_default_max_send_retries() -> u32 {
    paykit_lib::DEFAULT_MAX_SEND_RETRIES
}

/// Default maximum number of consecutive handshake recovery attempts.
#[uniffi::export]
pub fn paykit_default_max_recovery_attempts() -> u32 {
    paykit_lib::DEFAULT_MAX_RECOVERY_ATTEMPTS
}

/// Start an Encrypted Link Handshake as the initiator.
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

/// Start an Encrypted Link Handshake as the responder.
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

/// Advance an Encrypted Link Handshake by one polling-safe step.
///
/// Returns status `"pending"` with the same handshake handle, or `"complete"`
/// with a new Encrypted Link handle.
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
                reason: format!("Unknown Encrypted Link Handshake handle: {handshake_id}"),
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

/// Configure automatic recovery attempts for a pending Encrypted Link Handshake.
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
                reason: format!("Unknown Encrypted Link Handshake handle: {handshake_id}"),
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

/// Configure automatic send retries for an established Encrypted Link.
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
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        link.set_max_send_retries(max);
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Encrypt and send the complete Private Payment List over an established Encrypted Link.
#[uniffi::export]
pub async fn paykit_set_private_payment_list(
    link_id: String,
    list: FfiPrivatePaymentList,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let list = private_payment_list_to_lib(list)?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        paykit_lib::set_private_payment_list(link, &list).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Parse a Private Payment List JSON message.
#[uniffi::export]
pub fn paykit_parse_private_payment_list_json(
    json: String,
) -> Result<FfiPrivatePaymentList, PaykitFfiError> {
    let list = paykit_lib::parse_private_payment_list_json(&json)?;
    Ok(FfiPrivatePaymentList {
        payment_endpoints: payment_endpoints_map_to_ffi(list.payment_endpoints),
    })
}

/// Receive all currently available Private Application Messages from an established Encrypted Link.
#[uniffi::export]
pub async fn paykit_receive_private_application_messages(
    link_id: String,
) -> Result<Vec<FfiPrivateApplicationMessage>, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        let messages = link.receive_private_application_messages().await?;
        Ok(messages
            .into_iter()
            .map(private_application_message_to_ffi)
            .collect())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Parse a raw private stream message as a Payment Request Event Message.
#[uniffi::export]
pub fn paykit_parse_payment_request_event_message(
    message: FfiPrivateApplicationMessage,
) -> Result<Option<FfiPaymentRequestEventMessage>, PaykitFfiError> {
    let message = private_application_message_to_lib(message)?;
    paykit_lib::parse_payment_request_event_message(&message)
        .map(payment_request_event_message_to_ffi)
        .transpose()
}

/// Serialize a Payment Request Event Message to canonical JSON.
#[uniffi::export]
pub fn paykit_serialize_payment_request_event(
    event: FfiPaymentRequestEvent,
) -> Result<String, PaykitFfiError> {
    let event = payment_request_event_to_lib(event)?;
    Ok(paykit_lib::serialize_payment_request_event(&event)?)
}

/// Validate a Payment Proof against a Payment Request's immutable terms.
#[uniffi::export]
pub fn paykit_validate_payment_proof_for_request(
    proof: FfiPaymentProof,
    request: FfiPaymentRequest,
) -> Result<(), PaykitFfiError> {
    let proof = payment_proof_to_lib(proof)?;
    let request = payment_request_to_lib(request)?;
    Ok(proof.validate_for_request(&request)?)
}

/// Send a `paykit.payment_request` Event Message.
#[uniffi::export]
pub async fn paykit_send_payment_request(
    link_id: String,
    event: FfiPaymentRequest,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let event = payment_request_to_lib(event)?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        paykit_lib::send_payment_request(link, &event).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Send a `paykit.payment_request_acceptance` Event Message.
#[uniffi::export]
pub async fn paykit_send_payment_request_acceptance(
    link_id: String,
    event: FfiPaymentRequestAcceptance,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let event = payment_request_acceptance_to_lib(event)?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        paykit_lib::send_payment_request_acceptance(link, &event).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Send a `paykit.payment_request_rejection` Event Message.
#[uniffi::export]
pub async fn paykit_send_payment_request_rejection(
    link_id: String,
    event: FfiPaymentRequestRejection,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let event = payment_request_rejection_to_lib(event)?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        paykit_lib::send_payment_request_rejection(link, &event).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Send a `paykit.payment_request_cancellation` Event Message.
#[uniffi::export]
pub async fn paykit_send_payment_request_cancellation(
    link_id: String,
    event: FfiPaymentRequestCancellation,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let event = payment_request_cancellation_to_lib(event)?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        paykit_lib::send_payment_request_cancellation(link, &event).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Send a `paykit.payment_proof` Event Message.
#[uniffi::export]
pub async fn paykit_send_payment_proof(
    link_id: String,
    event: FfiPaymentProof,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let event = payment_proof_to_lib(event)?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        paykit_lib::send_payment_proof(link, &event).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Prepare a plaintext Receipt, Encrypted Receipt, and matching Receipt Access descriptor.
#[uniffi::export]
pub async fn paykit_prepare_receipt(
    link_id: String,
    draft: FfiReceiptDraft,
) -> Result<FfiPreparedReceipt, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let draft = receipt_draft_to_lib(draft)?;
        let handle = get_link_handle(link_id).await?;
        let guard = handle.lock().await;
        let link = guard.as_ref().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        let prepared = paykit_lib::prepare_receipt(link, draft)?;
        prepared_receipt_to_ffi(prepared)
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Store a prepared Encrypted Receipt at its Receipt Location.
#[uniffi::export]
pub async fn paykit_store_prepared_receipt(
    prepared: FfiPreparedReceipt,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let prepared = prepared_receipt_to_lib(prepared)?;
        let session = get_session().await?;
        paykit_lib::store_prepared_receipt(&session, &prepared).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Send a prepared Receipt Access descriptor over an established Encrypted Link.
#[uniffi::export]
pub async fn paykit_send_receipt_access(
    link_id: String,
    access: FfiReceiptAccess,
) -> Result<(), PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let access = receipt_access_to_lib(access)?;
        let handle = get_link_handle(link_id).await?;
        let mut guard = handle.lock().await;
        let link = guard.as_mut().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        paykit_lib::send_receipt_access(link, &access).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Parse a raw private stream message as a Receipt Access Event Message.
#[uniffi::export]
pub fn paykit_parse_receipt_access_event_message(
    message: FfiPrivateApplicationMessage,
) -> Result<Option<FfiReceiptAccessEventMessage>, PaykitFfiError> {
    let message = private_application_message_to_lib(message)?;
    paykit_lib::parse_receipt_access_event_message(&message)
        .map(receipt_access_event_message_to_ffi)
        .transpose()
}

/// Parse a Receipt Access JSON message.
#[uniffi::export]
pub fn paykit_parse_receipt_access_json(json: String) -> Result<FfiReceiptAccess, PaykitFfiError> {
    let access = paykit_lib::parse_receipt_access_json(&json)?;
    Ok(receipt_access_to_ffi(access))
}

/// Return the canonical homeserver Receipt Location path for a Receipt ID.
#[uniffi::export]
pub fn paykit_receipt_location(receipt_id: String) -> Result<String, PaykitFfiError> {
    let receipt_id = ReceiptId::new(receipt_id)?;
    Ok(ReceiptAccess::location_for(&receipt_id))
}

/// Decrypt an Encrypted Receipt fetched from the homeserver.
#[uniffi::export]
pub fn paykit_decrypt_receipt(
    encrypted_json: String,
    key: String,
    location: String,
) -> Result<FfiReceipt, PaykitFfiError> {
    let key = ReceiptDecryptionKey::new(key)?;
    let receipt = paykit_lib::decrypt_receipt(&encrypted_json, &key, &location)?;
    receipt_to_ffi(receipt)
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
                reason: format!("Unknown Encrypted Link Handshake handle: {handshake_id}"),
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

/// Serialize an established Encrypted Link snapshot for durable storage.
#[uniffi::export]
pub async fn paykit_serialize_encrypted_link(link_id: String) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let link_id = parse_handle_id(&link_id, "link")?;
        let handle = get_link_handle(link_id).await?;
        let guard = handle.lock().await;
        let link = guard.as_ref().ok_or_else(|| PaykitFfiError::Validation {
            reason: format!("Encrypted Link handle is closed: {link_id}"),
        })?;
        Ok(encode_snapshot(link.serialize()))
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Return the counterparty embedded in an Encrypted Link snapshot.
#[uniffi::export]
pub fn paykit_encrypted_link_snapshot_recipient(
    snapshot_hex: String,
) -> Result<String, PaykitFfiError> {
    let snapshot_bytes = decode_snapshot(&snapshot_hex, "Encrypted Link snapshot")?;
    let snapshot = EncryptedLinkSnapshot::deserialize(&snapshot_bytes)?;
    Ok(snapshot.recipient().to_string())
}

/// Return the counterparty embedded in an Encrypted Link Handshake snapshot.
#[uniffi::export]
pub fn paykit_encrypted_link_handshake_snapshot_recipient(
    snapshot_hex: String,
) -> Result<String, PaykitFfiError> {
    let snapshot_bytes = decode_snapshot(&snapshot_hex, "handshake snapshot")?;
    let snapshot = EncryptedLinkHandshakeSnapshot::deserialize(&snapshot_bytes)?;
    Ok(snapshot.recipient().to_string())
}

/// Restore an established Encrypted Link from a serialized snapshot.
#[uniffi::export]
pub async fn paykit_restore_encrypted_link(
    secret_key_hex: String,
    snapshot_hex: String,
) -> Result<String, PaykitFfiError> {
    let rt = ensure_runtime();
    rt.spawn(async move {
        let secret_key = parse_secret_key(&secret_key_hex)?;
        let snapshot_bytes = decode_snapshot(&snapshot_hex, "Encrypted Link snapshot")?;
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

/// Restore an in-progress Encrypted Link Handshake from a serialized snapshot.
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

/// Close an established Encrypted Link and remove its FFI handle.
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
                reason: format!("Unknown Encrypted Link handle: {link_id}"),
            })?;
        let link = handle
            .lock()
            .await
            .take()
            .ok_or_else(|| PaykitFfiError::Validation {
                reason: format!("Encrypted Link handle is closed: {link_id}"),
            })?;
        paykit_lib::close_encrypted_link(link).await?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(runtime_err(e)))
}

/// Drop an in-progress Encrypted Link Handshake handle.
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
                reason: format!("Unknown Encrypted Link Handshake handle: {handshake_id}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ffi_acceptance_event(event_type: &str) -> FfiPaymentRequestEvent {
        FfiPaymentRequestEvent {
            event_type: event_type.into(),
            request: None,
            acceptance: Some(FfiPaymentRequestAcceptance {
                event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
                payment_request_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            }),
            rejection: None,
            cancellation: None,
            proof: None,
        }
    }

    #[test]
    fn test_payment_request_event_to_lib_accepts_matching_variant() {
        let event = payment_request_event_to_lib(ffi_acceptance_event("acceptance")).unwrap();
        assert!(matches!(event, PaymentRequestEvent::Acceptance(_)));
    }

    #[test]
    fn test_payment_request_event_to_lib_rejects_mismatched_variant() {
        let err = payment_request_event_to_lib(ffi_acceptance_event("request")).unwrap_err();
        assert!(
            matches!(err, PaykitFfiError::Validation { ref reason } if reason.contains("must match event_type")),
            "expected validation error for mismatched variant, got: {err}"
        );
    }

    #[test]
    fn test_payment_request_event_to_lib_rejects_multiple_variants() {
        let mut event = ffi_acceptance_event("acceptance");
        event.rejection = Some(FfiPaymentRequestRejection {
            event_id: "750e8400-e29b-41d4-a716-446655440000".into(),
            payment_request_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            reason: None,
        });

        let err = payment_request_event_to_lib(event).unwrap_err();
        assert!(
            matches!(err, PaykitFfiError::Validation { ref reason } if reason.contains("exactly one variant")),
            "expected validation error for multiple variants, got: {err}"
        );
    }

    #[test]
    fn test_private_payment_list_to_lib_rejects_duplicate_identifiers() {
        let err = private_payment_list_to_lib(FfiPrivatePaymentList {
            payment_endpoints: vec![
                FfiPaymentEndpoint {
                    payment_endpoint_identifier: "btc-lightning-bolt11".into(),
                    payment_endpoint_payload: "first".into(),
                },
                FfiPaymentEndpoint {
                    payment_endpoint_identifier: "btc-lightning-bolt11".into(),
                    payment_endpoint_payload: "second".into(),
                },
            ],
        })
        .unwrap_err();

        assert!(
            matches!(err, PaykitFfiError::Validation { ref reason } if reason.contains("duplicate Payment Endpoint Identifier")),
            "expected validation error for duplicate identifiers, got: {err}"
        );
    }
}
