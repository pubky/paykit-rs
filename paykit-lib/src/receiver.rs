use serde::{Deserialize, Serialize};

use crate::{PaykitError, Result};

const PAYKIT_RECEIVER_PATH_MAX_LEN: usize = 128;
const PAYKIT_RECEIVER_SEGMENT_MAX_LEN: usize = 64;

/// App/runtime receiver path used in receiver-scoped Paykit storage paths.
///
/// A receiver path has the shape `{app}/{wallet|server}`, such as
/// `bitkit/wallet` or `bitkit/server`. It identifies one Paykit runtime folder
/// under a Pubky identity; it is not a global user identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PaykitReceiverPath(String);

impl PaykitReceiverPath {
    /// Create a validated Paykit receiver path.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_receiver_path(&value)?;
        Ok(Self(value))
    }

    /// Return the receiver path as a path-safe string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaykitReceiverPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PaykitReceiverPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PaykitReceiverPath {
    type Error = PaykitError;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<PaykitReceiverPath> for String {
    fn from(value: PaykitReceiverPath) -> Self {
        value.0
    }
}

fn validate_receiver_path(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(PaykitError::Validation(
            "PaykitReceiverPath must not be empty".into(),
        ));
    }
    if value.len() > PAYKIT_RECEIVER_PATH_MAX_LEN {
        return Err(PaykitError::Validation(format!(
            "PaykitReceiverPath must not exceed {PAYKIT_RECEIVER_PATH_MAX_LEN} bytes"
        )));
    }

    let segments: Vec<&str> = value.split('/').collect();
    if segments.len() != 2 {
        return Err(PaykitError::Validation(
            "PaykitReceiverPath must have the shape {app}/{wallet|server}".into(),
        ));
    }

    for segment in &segments {
        validate_receiver_path_segment(segment)?;
    }
    if segments[0] == "private" {
        return Err(PaykitError::Validation(
            "PaykitReceiverPath app segment 'private' is reserved".into(),
        ));
    }

    match segments[1] {
        "wallet" | "server" => Ok(()),
        runtime => Err(PaykitError::Validation(format!(
            "PaykitReceiverPath runtime segment must be 'wallet' or 'server', got '{runtime}'"
        ))),
    }
}

fn validate_receiver_path_segment(segment: &str) -> Result<()> {
    if segment.is_empty() {
        return Err(PaykitError::Validation(
            "PaykitReceiverPath segments must not be empty".into(),
        ));
    }
    if segment.len() > PAYKIT_RECEIVER_SEGMENT_MAX_LEN {
        return Err(PaykitError::Validation(format!(
            "PaykitReceiverPath segments must not exceed {PAYKIT_RECEIVER_SEGMENT_MAX_LEN} bytes"
        )));
    }
    if segment == "." || segment == ".." {
        return Err(PaykitError::Validation(
            "PaykitReceiverPath segments must not be path-traversal components".into(),
        ));
    }
    if segment
        .chars()
        .any(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'))
    {
        return Err(PaykitError::Validation(
            "PaykitReceiverPath segments may only contain lowercase ASCII letters, digits, and '-'"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_receiver_path_accepts_app_runtime_paths() {
        for value in [
            "bitkit/wallet",
            "bitkit/server",
            "paykit-server/server",
            "tether-wallet/wallet",
        ] {
            let path = PaykitReceiverPath::new(value).unwrap();
            assert_eq!(path.as_str(), value);
        }
    }

    #[test]
    fn test_receiver_path_rejects_unsafe_segments() {
        for value in [
            "",
            ".",
            "..",
            "bitkit",
            "bitkit/",
            "/wallet",
            "bitkit/ios",
            "bitkit/wallet/extra",
            "Bitkit/wallet",
            "bitkit_wallet/wallet",
            "bitkit/Wallet",
            "bitkit/../wallet",
            "private/wallet",
        ] {
            assert!(PaykitReceiverPath::new(value).is_err(), "{value}");
        }
    }
}
