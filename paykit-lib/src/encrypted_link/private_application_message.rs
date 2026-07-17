use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pubky::{errors::RequestError, Error as PubkyError, PubkySession, PublicKey, StatusCode};
use tracing::{debug, warn};

use crate::{
    encrypted_link::paths::compute_private_payment_paths,
    error::{NonRetryablePrivateReceiveError, NonRetryablePrivateSendError},
    PaykitError, PaykitReceiverPath, Result,
};

// Local marker used when decrypted private plaintext is not valid UTF-8.
// This is not a protocol message. It lets receive callers persist the malformed
// stream item, advance their local link checkpoint, and avoid wedging on the
// same encrypted slot forever.
const INVALID_UTF8_PRIVATE_MESSAGE_PREFIX: &str = "paykit.invalid_utf8_private_message:";
const LIST_PAGE_LIMIT: u16 = 100;

/// Private Message Kind values understood by Paykit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateMessageKind {
    /// Private Payment List Latest-State Message (`paykit.private_payment_list`).
    PrivatePaymentList,
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
            "paykit.receipt_access" => Some(Self::ReceiptAccess),
            "paykit.payment_request" => Some(Self::PaymentRequest),
            "paykit.payment_request_acceptance" => Some(Self::PaymentRequestAcceptance),
            "paykit.payment_request_rejection" => Some(Self::PaymentRequestRejection),
            "paykit.payment_request_cancellation" => Some(Self::PaymentRequestCancellation),
            "paykit.payment_proof" => Some(Self::PaymentProof),
            _ => None,
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
    /// Raw plaintext received over the Encrypted Link.
    pub raw_json: String,
}

impl std::fmt::Debug for PrivateApplicationMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateApplicationMessage")
            .field("version", &self.version)
            .field("kind", &self.kind)
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
        Self {
            version,
            kind,
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

fn decode_private_application_message(
    raw: &[u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN],
) -> Result<PrivateApplicationMessage> {
    // Trim trailing zero-padding added by pubky-noise's fixed-size buffers.
    // Paykit application messages are JSON, so trailing NUL bytes are not valid
    // payload content.
    let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let plaintext = match std::str::from_utf8(&raw[..end]) {
        Ok(plaintext) => plaintext.to_owned(),
        Err(_) => format!(
            "{INVALID_UTF8_PRIVATE_MESSAGE_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(&raw[..end])
        ),
    };

    Ok(PrivateApplicationMessage::from_plaintext(plaintext))
}

pub(super) async fn receive_private_application_messages(
    encryptor: &mut pubky_noise::PubkyNoiseEncryptor,
) -> Result<Vec<PrivateApplicationMessage>> {
    let mut messages = Vec::new();

    loop {
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
/// is not touched.
pub async fn clear_encrypted_link_outbox(
    session: &PubkySession,
    local_noise_secret_key: &[u8; 32],
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
) -> Result<usize> {
    let local_identity_public_key = session.info().public_key();
    let (write_path, _) = compute_private_payment_paths(
        local_noise_secret_key,
        local_identity_public_key,
        remote_identity_public_key,
        remote_noise_public_key,
        local_receiver_path,
        remote_receiver_path,
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

pub(super) async fn send_private_application_message(
    encryptor: &mut pubky_noise::PubkyNoiseEncryptor,
    max_send_retries: u32,
    plaintext: &[u8],
    context: &'static str,
) -> Result<()> {
    validate_private_application_message_size(plaintext, context)?;

    let max_attempts = send_attempts_from_retries(max_send_retries);
    let mut last_error: Option<String> = None;

    for attempt in 1..=max_attempts {
        match encryptor.send_message(plaintext).await {
            Ok(()) => {
                debug!(context, "Private Application Message sent successfully");
                return Ok(());
            }
            Err(err) if is_retryable_private_send_error(&err) => {
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
mod tests {
    use super::*;

    #[test]
    fn test_send_attempts_from_retries_bounds() {
        assert_eq!(send_attempts_from_retries(0), 1);
        assert_eq!(send_attempts_from_retries(3), 4);
        assert_eq!(send_attempts_from_retries(u32::MAX), u32::MAX);
    }

    #[test]
    fn test_private_send_retry_classification() {
        assert!(is_retryable_private_send_error(
            &pubky_noise::PubkyNoiseError::HomeserverWriteError,
        ));

        for err in [
            pubky_noise::PubkyNoiseError::IsHandshake,
            pubky_noise::PubkyNoiseError::EncryptionError,
            pubky_noise::PubkyNoiseError::CounterOverflow,
            pubky_noise::PubkyNoiseError::NonceOverflow,
        ] {
            assert!(
                !is_retryable_private_send_error(&err),
                "{err:?} should not be retried"
            );
        }
    }

    #[test]
    fn test_non_retryable_private_send_error_is_classified() {
        let err = PaykitError::Transport {
            context: "failed to send Private Application Message".into(),
            source: anyhow::Error::new(NonRetryablePrivateSendError(
                pubky_noise::PubkyNoiseError::EncryptionError,
            )),
        };

        assert!(err.is_non_retryable_private_send_error());
    }

    #[test]
    fn test_private_receive_retry_classification() {
        assert!(is_retryable_private_receive_error(
            &pubky_noise::PubkyNoiseError::HomeserverResponseError,
        ));

        for err in [
            pubky_noise::PubkyNoiseError::BadLengthCiphertext,
            pubky_noise::PubkyNoiseError::IsHandshake,
            pubky_noise::PubkyNoiseError::DecryptionError,
            pubky_noise::PubkyNoiseError::CounterOverflow,
            pubky_noise::PubkyNoiseError::NonceOverflow,
        ] {
            assert!(
                !is_retryable_private_receive_error(&err),
                "{err:?} should not be retried"
            );
        }
    }

    #[test]
    fn test_non_retryable_private_receive_error_is_classified() {
        let err = PaykitError::Transport {
            context: "failed to receive Private Application Messages".into(),
            source: anyhow::Error::new(NonRetryablePrivateReceiveError(
                pubky_noise::PubkyNoiseError::DecryptionError,
            )),
        };

        assert!(err.is_non_retryable_private_receive_error());
    }

    #[test]
    fn test_private_application_message_size_validation_rejects_oversized_payload() {
        let payload = vec![b'x'; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN + 1];
        let err =
            validate_private_application_message_size(&payload, "Payment Request").unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("exceeds")),
            "expected oversize validation error, got: {err}"
        );
    }

    #[test]
    fn test_private_application_message_keeps_malformed_header() {
        let raw = r#"{"version":"bad","kind":"paykit.payment_request","event_id":"not-a-uuid"}"#;
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string());

        assert_eq!(message.version, None);
        assert_eq!(
            message.known_kind(),
            Some(PrivateMessageKind::PaymentRequest)
        );
        assert_eq!(message.raw_json, raw);
    }

    #[test]
    fn test_private_application_message_keeps_json_without_kind() {
        let raw = r#"{"version":1,"payload":"unknown"}"#;
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string());

        assert_eq!(message.version, Some(1));
        assert_eq!(message.kind, None);
        assert_eq!(message.known_kind(), None);
        assert_eq!(message.raw_json, raw);
    }

    #[test]
    fn test_private_application_message_known_kind_uses_raw_json() {
        let raw = r#"{"version":1,"kind":"paykit.receipt_access"}"#;
        let message = PrivateApplicationMessage {
            version: Some(1),
            kind: Some("paykit.payment_request".to_string()),
            raw_json: raw.to_string(),
        };

        assert_eq!(
            message.known_kind(),
            Some(PrivateMessageKind::ReceiptAccess)
        );
    }

    #[test]
    fn test_private_application_message_debug_redacts_raw_json() {
        let raw = r#"{"version":1,"kind":"paykit.receipt_access","key":"secret"}"#;
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string());
        let debug = format!("{message:?}");

        assert!(!debug.contains("secret"));
        assert!(debug.contains("<redacted:"));
    }

    #[test]
    fn test_private_application_message_keeps_invalid_json() {
        let raw = "not json";
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string());

        assert_eq!(message.version, None);
        assert_eq!(message.kind, None);
        assert_eq!(message.known_kind(), None);
        assert_eq!(message.raw_json, raw);
    }

    #[test]
    fn test_decode_private_application_message_retains_invalid_utf8_marker() {
        let mut raw = [0u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN];
        raw[0] = 0xff;

        let message = decode_private_application_message(&raw).unwrap();

        assert_eq!(message.version, None);
        assert_eq!(message.kind, None);
        assert_eq!(message.known_kind(), None);
        assert_eq!(
            message.invalid_utf8_error(),
            Some("Private Application Message plaintext is not valid UTF-8")
        );
        assert!(message
            .raw_json
            .starts_with(INVALID_UTF8_PRIVATE_MESSAGE_PREFIX));
    }
}
