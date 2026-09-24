use serde::{Deserialize, Serialize};

use crate::{PaykitError, Result};

const PAYKIT_APP_ID_MAX_LEN: usize = 64;

/// Stable identifier for one application participating in a Paykit identity.
///
/// App IDs are used for endpoint ownership and private-message attribution.
/// They do not create separate Paykit identities or Encrypted Links.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PaykitAppId(String);

impl PaykitAppId {
    /// Create a validated Paykit App ID.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_app_id(&value)?;
        Ok(Self(value))
    }

    /// Return the path-safe App ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaykitAppId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PaykitAppId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PaykitAppId {
    type Error = PaykitError;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<PaykitAppId> for String {
    fn from(value: PaykitAppId) -> Self {
        value.0
    }
}

fn validate_app_id(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(PaykitError::Validation(
            "PaykitAppId must not be empty".into(),
        ));
    }
    if value.len() > PAYKIT_APP_ID_MAX_LEN {
        return Err(PaykitError::Validation(format!(
            "PaykitAppId must not exceed {PAYKIT_APP_ID_MAX_LEN} bytes"
        )));
    }
    if value == "." || value == ".." {
        return Err(PaykitError::Validation(
            "PaykitAppId must not be a path-traversal component".into(),
        ));
    }
    if value
        .chars()
        .any(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'))
    {
        return Err(PaykitError::Validation(
            "PaykitAppId may only contain lowercase ASCII letters, digits, and '-'".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_id_accepts_path_safe_values() {
        for value in ["bitkit", "paykit-server", "wallet-2"] {
            assert_eq!(PaykitAppId::new(value).unwrap().as_str(), value);
        }
    }

    #[test]
    fn test_app_id_rejects_unsafe_values() {
        for value in [
            "",
            ".",
            "..",
            "Bitkit",
            "bitkit/wallet",
            "bitkit_wallet",
            "bitkit.wallet",
        ] {
            assert!(PaykitAppId::new(value).is_err(), "{value}");
        }
    }
}
