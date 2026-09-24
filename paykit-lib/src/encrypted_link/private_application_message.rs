use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pubky::{errors::RequestError, Error as PubkyError, PubkySession, PublicKey, StatusCode};
use tracing::{debug, warn};

use crate::{
    encrypted_link::paths::compute_private_payment_paths,
    error::{NonRetryablePrivateReceiveError, NonRetryablePrivateSendError},
    PaykitError, Result,
};

// Local marker used when decrypted private plaintext is not valid UTF-8.
// This is not a protocol message. It lets receive callers persist the malformed
// stream item, advance their local link checkpoint, and avoid wedging on the
// same encrypted slot forever.
const INVALID_UTF8_PRIVATE_MESSAGE_PREFIX: &str = "paykit.invalid_utf8_private_message:";
const LIST_PAGE_LIMIT: u16 = 100;
/// Maximum encrypted stream slots consumed by one receive call.
pub const PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT: usize = 100;

/// Private Message Kind values understood by Paykit.
///
/// This enum is intentionally exhaustive. Adding a variant must produce
/// compile-time failures in routing, validation, and backup-validation matches
/// until the new Private Message Kind is classified explicitly. Do not add
/// `#[non_exhaustive]` without a coordinated team decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateMessageKind {
    /// Private Payment List Latest-State Message (`paykit.private_payment_list`).
    PrivatePaymentList,
    /// Delivery Confirmation (`paykit.delivery_confirmation`), not an Event Message.
    DeliveryConfirmation,
    /// Receipt Access Event Message (`paykit.receipt_access`).
    ReceiptAccess,
    /// Payment Request Event Message (`paykit.payment_request`).
    PaymentRequest,
    /// Payment Request Acceptance Event Message (`paykit.payment_request_acceptance`).
    PaymentRequestAcceptance,
    /// Payment Request Rejection Event Message (`paykit.payment_request_rejection`).
    PaymentRequestRejection,
    /// Payment Request Cancellation Event Message (`paykit.payment_request_cancellation`).
    PaymentRequestCancellation,
    /// Payment Proof Event Message (`paykit.payment_proof`).
    PaymentProof,
}

impl PrivateMessageKind {
    /// Return the canonical private message kind string used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrivatePaymentList => "paykit.private_payment_list",
            Self::DeliveryConfirmation => "paykit.delivery_confirmation",
            Self::ReceiptAccess => "paykit.receipt_access",
            Self::PaymentRequest => "paykit.payment_request",
            Self::PaymentRequestAcceptance => "paykit.payment_request_acceptance",
            Self::PaymentRequestRejection => "paykit.payment_request_rejection",
            Self::PaymentRequestCancellation => "paykit.payment_request_cancellation",
            Self::PaymentProof => "paykit.payment_proof",
        }
    }

    /// Parse a canonical private message kind string.
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "paykit.private_payment_list" => Some(Self::PrivatePaymentList),
            "paykit.delivery_confirmation" => Some(Self::DeliveryConfirmation),
            "paykit.receipt_access" => Some(Self::ReceiptAccess),
            "paykit.payment_request" => Some(Self::PaymentRequest),
            "paykit.payment_request_acceptance" => Some(Self::PaymentRequestAcceptance),
            "paykit.payment_request_rejection" => Some(Self::PaymentRequestRejection),
            "paykit.payment_request_cancellation" => Some(Self::PaymentRequestCancellation),
            "paykit.payment_proof" => Some(Self::PaymentProof),
            _ => None,
        }
    }

    /// Whether this kind carries an Event Message with its own Event ID.
    ///
    /// Delivery Confirmations refer to an existing event and must not themselves
    /// receive confirmations. Private Payment Lists use Latest-State semantics.
    pub fn is_event(self) -> bool {
        match self {
            Self::ReceiptAccess
            | Self::PaymentRequest
            | Self::PaymentRequestAcceptance
            | Self::PaymentRequestRejection
            | Self::PaymentRequestCancellation
            | Self::PaymentProof => true,
            Self::PrivatePaymentList | Self::DeliveryConfirmation => false,
        }
    }
}

impl std::fmt::Display for PrivateMessageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One Private Application Message received from an Encrypted Link.
///
/// This is the low-level receive item for the Private Application Message stream.
/// It keeps the raw plaintext payload so callers can route and persist messages
/// themselves. If decrypted bytes are not UTF-8, `raw_json` contains a local
/// invalid-JSON marker with the bytes encoded as base64url. Higher layers should
/// treat that marker as malformed input, not as a Private Application Message.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateApplicationMessage {
    /// Private Application Message version from the JSON `version` field, when
    /// the field is present and can be represented as a `u8`.
    pub version: Option<u8>,
    /// Message kind string from the JSON `kind` field, when the field is a
    /// string.
    pub kind: Option<String>,
    /// App ID string from the JSON `app_id` field, when the field is a string.
    pub app_id: Option<String>,
    /// Raw plaintext received over the Encrypted Link.
    pub raw_json: String,
}

impl std::fmt::Debug for PrivateApplicationMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateApplicationMessage")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field("app_id", &self.app_id)
            .field(
                "raw_json",
                &format_args!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .finish()
    }
}

impl PrivateApplicationMessage {
    pub(super) fn from_plaintext(plaintext: String) -> Self {
        let value = serde_json::from_str::<serde_json::Value>(&plaintext).ok();
        let version = value
            .as_ref()
            .and_then(|value| value.get("version"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u8::try_from(version).ok());
        let kind = value
            .as_ref()
            .and_then(|value| value.get("kind"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let app_id = value
            .as_ref()
            .and_then(|value| value.get("app_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Self {
            version,
            kind,
            app_id,
            raw_json: plaintext,
        }
    }

    /// Return the known Paykit kind from the raw payload, if this library can
    /// parse and recognize it.
    pub fn known_kind(&self) -> Option<PrivateMessageKind> {
        serde_json::from_str::<serde_json::Value>(&self.raw_json)
            .ok()
            .and_then(|value| {
                value
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .and_then(PrivateMessageKind::parse)
            })
    }

    /// Return a parse error for the local invalid-UTF-8 receive marker.
    ///
    /// SDK/runtime callers use this to persist an audit/error record for a
    /// malformed stream item while still advancing the local Encrypted Link
    /// checkpoint. This is not expected on valid protocol messages.
    pub fn invalid_utf8_error(&self) -> Option<&'static str> {
        self.raw_json
            .starts_with(INVALID_UTF8_PRIVATE_MESSAGE_PREFIX)
            .then_some("Private Application Message plaintext is not valid UTF-8")
    }
}

pub(super) fn decode_private_application_message(raw: &[u8]) -> Result<PrivateApplicationMessage> {
    // pubky-noise returns the exact authenticated application body. It owns
    // transport framing and padding; preserve malformed JSON for caller routing.
    let plaintext = match std::str::from_utf8(raw) {
        Ok(plaintext) => plaintext.to_owned(),
        Err(_) => format!(
            "{INVALID_UTF8_PRIVATE_MESSAGE_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(raw)
        ),
    };

    Ok(PrivateApplicationMessage::from_plaintext(plaintext))
}

pub(super) async fn receive_private_application_messages(
    encryptor: &mut pubky_noise::PubkyNoiseEncryptor,
) -> Result<Vec<PrivateApplicationMessage>> {
    let mut messages = Vec::new();

    while messages.len() < PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT {
        let batch = encryptor.receive_message().await.map_err(|err| {
            let context = format!("failed to receive Private Application Messages: {err:?}");
            let source = if is_retryable_private_receive_error(&err) {
                anyhow::anyhow!("pubky-noise receive_message failed: {err:?}")
            } else {
                anyhow::Error::new(NonRetryablePrivateReceiveError(err))
            };
            PaykitError::Transport { context, source }
        })?;

        if batch.is_empty() {
            break;
        }

        for raw in batch {
            messages.push(decode_private_application_message(&raw)?);
        }
    }

    Ok(messages)
}

/// Delete all encrypted stream slots written by the local identity for one
/// counterparty.
///
/// This clears the local write path used by the counterparty as their read path.
/// It is intended for recovery before starting a fresh Encrypted Link Handshake
/// after the previous link state has been abandoned. The counterparty's outbox
/// is not touched. The local Pubky identity comes from `session`; the remote
/// identity and Noise key must belong to the intended counterparty. The caller
/// owns session creation, capability scope, key rotation, and request timeouts.
pub async fn clear_encrypted_link_outbox(
    session: &PubkySession,
    local_secret_key: &[u8; 32],
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
) -> Result<usize> {
    let (write_path, _) = compute_private_payment_paths(
        local_secret_key,
        session.info().public_key(),
        remote_identity_public_key,
        remote_noise_public_key,
    );
    let list_path = format!("{write_path}/");
    let storage = session.storage();
    let mut deleted_count = 0;

    loop {
        let builder = match storage.list(&list_path) {
            Ok(builder) => builder.shallow(true).limit(LIST_PAGE_LIMIT),
            Err(err) if is_not_found(&err) => return Ok(deleted_count),
            Err(err) => {
                return Err(PaykitError::Transport {
                    context: "list Encrypted Link outbox".into(),
                    source: err.into(),
                });
            }
        };

        let page = match builder.send().await {
            Ok(page) => page,
            Err(err) if is_not_found(&err) => return Ok(deleted_count),
            Err(err) => {
                return Err(PaykitError::Transport {
                    context: "list Encrypted Link outbox".into(),
                    source: err.into(),
                });
            }
        };

        if page.is_empty() {
            return Ok(deleted_count);
        }

        let page_len = page.len();
        for resource in page {
            match storage.delete(resource.path.as_str()).await {
                Ok(_) => deleted_count += 1,
                Err(err) if is_not_found(&err) => {}
                Err(err) => {
                    return Err(PaykitError::Transport {
                        context: "clear Encrypted Link outbox".into(),
                        source: err.into(),
                    });
                }
            }
        }

        if page_len < LIST_PAGE_LIMIT as usize {
            return Ok(deleted_count);
        }
    }
}

fn send_attempts_from_retries(max_send_retries: u32) -> u32 {
    max_send_retries.saturating_add(1)
}

fn is_retryable_private_send_error(err: &pubky_noise::PubkyNoiseError) -> bool {
    matches!(err, pubky_noise::PubkyNoiseError::HomeserverWriteError)
}

fn is_retryable_private_receive_error(err: &pubky_noise::PubkyNoiseError) -> bool {
    matches!(err, pubky_noise::PubkyNoiseError::HomeserverResponseError)
}

fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}

fn validate_private_application_message_size(
    plaintext: &[u8],
    context: &'static str,
) -> Result<()> {
    if plaintext.len() > pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN {
        return Err(PaykitError::Validation(format!(
            "{context} payload ({} bytes) exceeds max message size ({} bytes)",
            plaintext.len(),
            pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
        )));
    }
    Ok(())
}

#[allow(
    deprecated,
    reason = "the stateless convenience API cannot persist a prepared send"
)]
pub(super) async fn send_private_application_message(
    encryptor: &mut pubky_noise::PubkyNoiseEncryptor,
    max_send_retries: u32,
    plaintext: &[u8],
    context: &'static str,
) -> Result<()> {
    validate_private_application_message_size(plaintext, context)?;

    let max_attempts = send_attempts_from_retries(max_send_retries);
    let mut last_error: Option<String> = None;
    // A failed outbox write leaves the prepared packet unacknowledged.
    // Preparing a new send would then fail with
    // `UnacknowledgedPreparedTransport` (non-retryable), so retries republish
    // the same prepared packet instead of preparing a new one.
    let mut retry_pending = false;

    for attempt in 1..=max_attempts {
        let outcome = if retry_pending {
            encryptor.retry_pending_send().await
        } else {
            encryptor.send_message(plaintext).await
        };
        match outcome {
            Ok(()) => {
                debug!(context, "Private Application Message sent successfully");
                return Ok(());
            }
            Err(err) if is_retryable_private_send_error(&err) => {
                retry_pending = true;
                last_error = Some(format!("{err:?}"));
                if attempt < max_attempts {
                    warn!(
                        attempt,
                        max_retries = max_send_retries,
                        error = ?err,
                        context,
                        "send_message failed, retrying"
                    );
                }
            }
            Err(err) => {
                let context = format!("failed to send {context}: {err:?}");
                return Err(PaykitError::Transport {
                    context,
                    source: anyhow::Error::new(NonRetryablePrivateSendError(err)),
                });
            }
        }
    }

    Err(PaykitError::Transport {
        context: format!("failed to send {context} after {max_attempts} attempts"),
        source: anyhow::anyhow!(
            "pubky-noise send_message failed on all {} attempts; last error: {}",
            max_attempts,
            last_error.unwrap_or_else(|| "unknown error".to_string())
        ),
    })
}

#[cfg(test)]
mod tests;
