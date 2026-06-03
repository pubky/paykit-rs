use tracing::{debug, warn};

use crate::{PaykitError, Result};

/// Private Message Kind values understood by Paykit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateMessageKind {
    /// Private Payment List Latest-State Message (`paykit.private_payment_list`).
    PrivatePaymentList,
    /// Receipt Access Event Message (`paykit.receipt_access`).
    ReceiptAccess,
}

impl PrivateMessageKind {
    /// Return the canonical private message kind string used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrivatePaymentList => "paykit.private_payment_list",
            Self::ReceiptAccess => "paykit.receipt_access",
        }
    }

    /// Parse a canonical private message kind string.
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "paykit.private_payment_list" => Some(Self::PrivatePaymentList),
            "paykit.receipt_access" => Some(Self::ReceiptAccess),
            _ => None,
        }
    }
}

impl std::fmt::Display for PrivateMessageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One private Paykit application message received from an Encrypted Link.
///
/// This is the low-level receive item for the private message stream. It keeps
/// the raw JSON so callers can route and persist messages themselves.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateApplicationMessage {
    /// Private Application Message version from the JSON `version` field, when
    /// the field is present and can be represented as a `u8`.
    pub version: Option<u8>,
    /// Message kind string from the JSON `kind` field, when the field is a
    /// string.
    pub kind: Option<String>,
    /// Raw JSON plaintext received over the Encrypted Link.
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
    pub(super) fn from_plaintext(plaintext: String) -> Result<Self> {
        let value: serde_json::Value =
            serde_json::from_str(&plaintext).map_err(|err| PaykitError::InvalidData {
                context: format!("failed to parse private message JSON: {err}"),
                source: Some(err.into()),
            })?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u8::try_from(version).ok());
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok(Self {
            version,
            kind,
            raw_json: plaintext,
        })
    }

    /// Return the known Paykit kind from the raw JSON payload, if this library
    /// version recognizes it.
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
}

fn decode_private_application_message(
    raw: &[u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN],
) -> Result<PrivateApplicationMessage> {
    // Trim trailing zero-padding added by pubky-noise's fixed-size buffers.
    // Paykit application messages are JSON, so trailing NUL bytes are not valid
    // payload content.
    let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let plaintext = std::str::from_utf8(&raw[..end]).map_err(|err| PaykitError::InvalidData {
        context: format!("private message plaintext is not valid UTF-8: {err}"),
        source: Some(err.into()),
    })?;

    PrivateApplicationMessage::from_plaintext(plaintext.to_owned())
}

pub(super) async fn receive_private_application_messages(
    encryptor: &mut pubky_noise::PubkyNoiseEncryptor,
) -> Result<Vec<PrivateApplicationMessage>> {
    let mut received = 0usize;
    let mut malformed = 0usize;
    let mut messages = Vec::new();

    loop {
        let batch = encryptor
            .receive_message()
            .await
            .map_err(|err| PaykitError::Transport {
                context: format!("failed to receive private messages: {err:?}"),
                source: anyhow::anyhow!("pubky-noise receive_message failed: {err:?}"),
            })?;

        if batch.is_empty() {
            break;
        }

        received += batch.len();
        for raw in batch {
            match decode_private_application_message(&raw) {
                Ok(message) => messages.push(message),
                Err(err) => {
                    malformed += 1;
                    warn!(
                        error = ?err,
                        "dropping malformed Private Application Message"
                    );
                }
            }
        }
    }

    if malformed > 0 {
        warn!(
            received,
            malformed,
            "ignored malformed Private Application Messages while preserving later messages"
        );
    }
    Ok(messages)
}

fn send_attempts_from_retries(max_send_retries: u32) -> u32 {
    max_send_retries.saturating_add(1)
}

fn is_retryable_private_send_error(err: &pubky_noise::PubkyNoiseError) -> bool {
    matches!(err, pubky_noise::PubkyNoiseError::HomeserverWriteError)
}

fn validate_private_application_message_size(plaintext: &[u8], context: &'static str) -> Result<()> {
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
                debug!(context, "private message sent successfully");
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
                return Err(PaykitError::Transport {
                    context: format!("failed to send {context}: {err:?}"),
                    source: anyhow::anyhow!(
                        "pubky-noise send_message failed with non-retryable error: {err:?}"
                    ),
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
    fn test_private_application_message_size_validation_rejects_oversized_payload() {
        let payload = vec![b'x'; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN + 1];
        let err = validate_private_application_message_size(&payload, "Private Payment List").unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("exceeds")),
            "expected oversize validation error, got: {err}"
        );
    }

    #[test]
    fn test_private_application_message_keeps_malformed_header() {
        let raw = r#"{"version":"bad","kind":"paykit.private_payment_list"}"#;
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string()).unwrap();

        assert_eq!(message.version, None);
        assert_eq!(
            message.known_kind(),
            Some(PrivateMessageKind::PrivatePaymentList)
        );
        assert_eq!(message.raw_json, raw);
    }

    #[test]
    fn test_private_application_message_keeps_json_without_kind() {
        let raw = r#"{"version":1,"payload":"unknown"}"#;
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string()).unwrap();

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
            kind: Some("paykit.private_payment_list".to_string()),
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
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string()).unwrap();
        let debug = format!("{message:?}");

        assert!(!debug.contains("secret"));
        assert!(debug.contains("<redacted:"));
    }
}
