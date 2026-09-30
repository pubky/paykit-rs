use crate::{validation::validate_uuid_v4, PaykitError, Result};

/// Recovery attempts bound to one Encrypted Link, in the local peer's order.
///
/// Swapping peers also swaps these IDs. An absent ID denotes a peer that has
/// not published a recovery marker. IDs are public metadata, not key material.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncryptedLinkRecoveryContext {
    local_attempt_id: Option<String>,
    remote_attempt_id: Option<String>,
}

impl EncryptedLinkRecoveryContext {
    /// Validate the local and remote UUID-v4 recovery attempt IDs.
    pub fn new(local_attempt_id: Option<&str>, remote_attempt_id: Option<&str>) -> Result<Self> {
        let validate =
            |id: &str| validate_uuid_v4(id.to_owned(), "Encrypted Link recovery attempt ID");
        Ok(Self {
            local_attempt_id: local_attempt_id.map(validate).transpose()?,
            remote_attempt_id: remote_attempt_id.map(validate).transpose()?,
        })
    }

    /// Local peer's recovery attempt, if published.
    pub fn local_attempt_id(&self) -> Option<&str> {
        self.local_attempt_id.as_deref()
    }

    /// Remote peer's recovery attempt, if published.
    pub fn remote_attempt_id(&self) -> Option<&str> {
        self.remote_attempt_id.as_deref()
    }

    pub(super) const ENCODED_LEN: usize = 72;

    pub(super) fn append_bytes(&self, bytes: &mut Vec<u8>, local_first: bool) {
        let ids = if local_first {
            [self.local_attempt_id(), self.remote_attempt_id()]
        } else {
            [self.remote_attempt_id(), self.local_attempt_id()]
        };
        for id in ids {
            bytes.extend_from_slice(id.map(str::as_bytes).unwrap_or(&[0; 36]));
        }
    }

    pub(super) fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let invalid = || PaykitError::InvalidData {
            context: "invalid Encrypted Link recovery context".into(),
            source: None,
        };
        if bytes.len() != Self::ENCODED_LEN {
            return Err(invalid());
        }
        fn decode(bytes: &[u8]) -> Result<Option<&str>> {
            if bytes == [0; 36] {
                Ok(None)
            } else {
                std::str::from_utf8(bytes)
                    .map(Some)
                    .map_err(|err| PaykitError::InvalidData {
                        context: "invalid Encrypted Link recovery attempt ID".into(),
                        source: Some(err.into()),
                    })
            }
        }
        Self::new(decode(&bytes[..36])?, decode(&bytes[36..])?).map_err(|_| invalid())
    }
}
