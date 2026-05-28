use std::collections::VecDeque;

use serde::Deserialize;
use tracing::{debug, warn};

use crate::{EncryptedLink, PaykitError, Result};

/// Private Noise message kinds understood by Paykit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateMessageKind {
    /// Private Payment Envelope Latest-State Message (`paykit.private_payment_envelope`).
    PrivatePaymentEnvelope,
    /// Receipt Access Event Message (`paykit.receipt_access`).
    ReceiptAccess,
}

impl PrivateMessageKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PrivatePaymentEnvelope => "paykit.private_payment_envelope",
            Self::ReceiptAccess => "paykit.receipt_access",
        }
    }

    pub(crate) fn is_supported(kind: &str) -> bool {
        kind == Self::PrivatePaymentEnvelope.as_str() || kind == Self::ReceiptAccess.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BufferedPrivateMessage {
    pub(crate) kind: String,
    pub(crate) plaintext: String,
}

impl BufferedPrivateMessage {
    pub(crate) fn is_kind(&self, kind: PrivateMessageKind) -> bool {
        self.kind == kind.as_str()
    }
}

#[derive(Deserialize)]
struct PrivateMessageHeader {
    kind: String,
}

fn decode_private_message(
    raw: &[u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN],
) -> Result<BufferedPrivateMessage> {
    // Trim trailing zero-padding added by pubky-noise's fixed-size buffers.
    // Paykit application messages are JSON, so trailing NUL bytes are not valid
    // payload content.
    let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let plaintext = std::str::from_utf8(&raw[..end]).map_err(|err| PaykitError::InvalidData {
        context: format!("private message plaintext is not valid UTF-8: {err}"),
        source: Some(err.into()),
    })?;

    let header: PrivateMessageHeader =
        serde_json::from_str(plaintext).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to parse private message header JSON: {err}"),
            source: Some(err.into()),
        })?;

    Ok(BufferedPrivateMessage {
        kind: header.kind,
        plaintext: plaintext.to_owned(),
    })
}

pub(crate) async fn receive_private_messages(link: &mut EncryptedLink) -> Result<usize> {
    let mut received = 0usize;
    let mut malformed = 0usize;
    let mut unknown = 0usize;

    loop {
        let messages =
            link.encryptor
                .receive_message()
                .await
                .map_err(|err| PaykitError::Transport {
                    context: format!("failed to receive private messages: {err:?}"),
                    source: anyhow::anyhow!("pubky-noise receive_message failed: {err:?}"),
                })?;

        if messages.is_empty() {
            break;
        }

        received += messages.len();
        for raw in messages {
            match decode_private_message(&raw) {
                Ok(message) if PrivateMessageKind::is_supported(&message.kind) => {
                    link.pending_private_messages.push_back(message)
                }
                Ok(message) => {
                    unknown += 1;
                    warn!(
                        kind = %message.kind,
                        "dropping unsupported Private Application Message kind"
                    );
                }
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
            "ignored malformed Private Application Messages while preserving later valid messages"
        );
    }
    if unknown > 0 {
        warn!(
            received,
            unknown, "dropped unsupported Private Application Message kinds"
        );
    }

    Ok(received)
}

pub(crate) fn take_latest_pending_message(
    pending: &mut VecDeque<BufferedPrivateMessage>,
    kind: PrivateMessageKind,
) -> Option<BufferedPrivateMessage> {
    let mut retained = VecDeque::with_capacity(pending.len());
    let mut latest = None;

    while let Some(message) = pending.pop_front() {
        if message.is_kind(kind) {
            latest = Some(message);
        } else {
            retained.push_back(message);
        }
    }

    *pending = retained;
    latest
}

pub(crate) fn take_all_pending_messages(
    pending: &mut VecDeque<BufferedPrivateMessage>,
    kind: PrivateMessageKind,
) -> Vec<BufferedPrivateMessage> {
    let mut retained = VecDeque::with_capacity(pending.len());
    let mut selected = Vec::new();

    while let Some(message) = pending.pop_front() {
        if message.is_kind(kind) {
            selected.push(message);
        } else {
            retained.push_back(message);
        }
    }

    *pending = retained;
    selected
}

pub(crate) fn send_attempts_from_retries(max_send_retries: u32) -> u32 {
    max_send_retries.saturating_add(1)
}

pub(crate) fn is_retryable_private_send_error(err: &pubky_noise::PubkyNoiseError) -> bool {
    matches!(err, pubky_noise::PubkyNoiseError::HomeserverWriteError)
}

pub(crate) async fn send_private_message(
    link: &mut EncryptedLink,
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

    let max_attempts = send_attempts_from_retries(link.max_send_retries);
    let mut last_error: Option<String> = None;

    for attempt in 1..=max_attempts {
        match link.encryptor.send_message(plaintext).await {
            Ok(()) => {
                debug!(context, "private message sent successfully");
                return Ok(());
            }
            Err(err) if is_retryable_private_send_error(&err) => {
                last_error = Some(format!("{err:?}"));
                if attempt < max_attempts {
                    warn!(
                        attempt,
                        max_retries = link.max_send_retries,
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
}
