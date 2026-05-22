use std::collections::VecDeque;

use tracing::{debug, warn};

use crate::{PaykitError, PrivateMessageHeader, PrivateMessageKind, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BufferedPrivateMessage {
    kind: PrivateMessageKind,
    plaintext: String,
}

impl BufferedPrivateMessage {
    #[cfg(test)]
    pub(crate) fn kind(&self) -> PrivateMessageKind {
        self.kind
    }

    pub(crate) fn plaintext(&self) -> &str {
        &self.plaintext
    }

    fn is_kind(&self, kind: PrivateMessageKind) -> bool {
        self.kind == kind
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrivateReceiveStats {
    pub(crate) received: usize,
    pub(crate) malformed: usize,
    pub(crate) unsupported: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PrivateMessageInbox {
    pending: VecDeque<BufferedPrivateMessage>,
}

impl PrivateMessageInbox {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn len(&self) -> usize {
        self.pending.len()
    }

    #[cfg(test)]
    pub(crate) fn kind_at(&self, index: usize) -> Option<PrivateMessageKind> {
        self.pending.get(index).map(BufferedPrivateMessage::kind)
    }

    #[cfg(test)]
    pub(crate) fn push_for_test(&mut self, kind: PrivateMessageKind, plaintext: String) {
        self.pending
            .push_back(BufferedPrivateMessage { kind, plaintext });
    }

    pub(crate) async fn receive_available(
        &mut self,
        encryptor: &mut pubky_noise::PubkyNoiseEncryptor,
    ) -> Result<PrivateReceiveStats> {
        let mut stats = PrivateReceiveStats::default();

        loop {
            let messages =
                encryptor
                    .receive_message()
                    .await
                    .map_err(|err| PaykitError::Transport {
                        context: format!("failed to receive private messages: {err:?}"),
                        source: anyhow::anyhow!("pubky-noise receive_message failed: {err:?}"),
                    })?;

            if messages.is_empty() {
                break;
            }

            stats.received += messages.len();
            for raw in messages {
                match decode_private_message(&raw) {
                    Ok(message) => self.pending.push_back(message),
                    Err(PrivateMessageDecodeError::Unsupported { kind }) => {
                        stats.unsupported += 1;
                        warn!(kind = %kind, "dropping unsupported private application message kind");
                    }
                    Err(PrivateMessageDecodeError::Malformed { error }) => {
                        stats.malformed += 1;
                        warn!(
                            error = ?error,
                            "dropping malformed private application message"
                        );
                    }
                }
            }
        }

        if stats.malformed > 0 {
            warn!(
                received = stats.received,
                malformed = stats.malformed,
                "ignored malformed private application messages while preserving later valid messages"
            );
        }
        if stats.unsupported > 0 {
            warn!(
                received = stats.received,
                unsupported = stats.unsupported,
                "dropped unsupported private application message kinds"
            );
        }

        Ok(stats)
    }

    pub(crate) fn take_latest(
        &mut self,
        kind: PrivateMessageKind,
    ) -> Option<BufferedPrivateMessage> {
        let mut retained = VecDeque::with_capacity(self.pending.len());
        let mut latest = None;

        while let Some(message) = self.pending.pop_front() {
            if message.is_kind(kind) {
                latest = Some(message);
            } else {
                retained.push_back(message);
            }
        }

        self.pending = retained;
        latest
    }

    pub(crate) fn take_all_fifo(
        &mut self,
        kind: PrivateMessageKind,
    ) -> Vec<BufferedPrivateMessage> {
        let mut retained = VecDeque::with_capacity(self.pending.len());
        let mut selected = Vec::new();

        while let Some(message) = self.pending.pop_front() {
            if message.is_kind(kind) {
                selected.push(message);
            } else {
                retained.push_back(message);
            }
        }

        self.pending = retained;
        selected
    }
}

enum PrivateMessageDecodeError {
    Unsupported { kind: String },
    Malformed { error: PaykitError },
}

fn decode_private_message(
    raw: &[u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN],
) -> std::result::Result<BufferedPrivateMessage, PrivateMessageDecodeError> {
    // Trim trailing zero-padding added by pubky-noise's fixed-size buffers.
    // Paykit application messages are JSON, so trailing NUL bytes are not valid
    // payload content.
    let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let plaintext =
        std::str::from_utf8(&raw[..end]).map_err(|err| PrivateMessageDecodeError::Malformed {
            error: PaykitError::InvalidData {
                context: format!("private message plaintext is not valid UTF-8: {err}"),
                source: Some(err.into()),
            },
        })?;

    let header: PrivateMessageHeader =
        serde_json::from_str(plaintext).map_err(|err| PrivateMessageDecodeError::Malformed {
            error: PaykitError::InvalidData {
                context: format!("failed to parse private message header JSON: {err}"),
                source: Some(err.into()),
            },
        })?;

    let Some(kind) = PrivateMessageKind::from_wire_name(&header.kind) else {
        return Err(PrivateMessageDecodeError::Unsupported { kind: header.kind });
    };

    debug!(kind = %kind.as_str(), "buffering supported private application message");
    Ok(BufferedPrivateMessage {
        kind,
        plaintext: plaintext.to_owned(),
    })
}
