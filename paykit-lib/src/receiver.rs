use serde::{Deserialize, Serialize};

use crate::{PaykitError, Result};

const PAYKIT_RECEIVER_ID_MAX_LEN: usize = 64;

/// App/runtime receiver id used in receiver-scoped Paykit storage paths.
///
/// A receiver id is a single path segment such as `bitkit`, `tether`, or
/// `bitkit-9f3a`. It identifies the Paykit runtime folder under one Pubky
/// identity; it is not a global user identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PaykitReceiverId(String);

impl PaykitReceiverId {
    /// Create a validated Paykit receiver id.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_receiver_id(&value)?;
        Ok(Self(value))
    }

    /// Return the receiver id as a path-safe string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaykitReceiverId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PaykitReceiverId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PaykitReceiverId {
    type Error = PaykitError;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<PaykitReceiverId> for String {
    fn from(value: PaykitReceiverId) -> Self {
        value.0
    }
}

fn validate_receiver_id(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(PaykitError::Validation(
            "PaykitReceiverId must not be empty".into(),
        ));
    }
    if value.len() > PAYKIT_RECEIVER_ID_MAX_LEN {
        return Err(PaykitError::Validation(format!(
            "PaykitReceiverId must not exceed {PAYKIT_RECEIVER_ID_MAX_LEN} bytes"
        )));
    }
    if value == "." || value == ".." {
        return Err(PaykitError::Validation(
            "PaykitReceiverId must not be a path-traversal component".into(),
        ));
    }
    if value
        .chars()
        .any(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'))
    {
        return Err(PaykitError::Validation(
            "PaykitReceiverId may only contain lowercase ASCII letters, digits, and '-'".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_receiver_id_accepts_readable_runtime_ids() {
        for value in ["paykit", "bitkit", "bitkit-9f3a", "tether-wallet"] {
            let id = PaykitReceiverId::new(value).unwrap();
            assert_eq!(id.as_str(), value);
        }
    }

    #[test]
    fn test_receiver_id_rejects_unsafe_segments() {
        for value in ["", ".", "..", "Bitkit", "bitkit/ios", "bitkit_ios"] {
            assert!(PaykitReceiverId::new(value).is_err(), "{value}");
        }
    }
}
