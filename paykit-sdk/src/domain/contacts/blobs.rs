use serde::{Deserialize, Serialize};

use crate::{PaykitSdkError, PubkyPublicKey, Result};

pub(crate) const PAYKIT_PROFILE_BLOB_PATH_PREFIX: &str = "/pub/paykit/blobs/";
const MAX_PAYKIT_BLOB_NAME_BYTES: usize = 128;

/// Public blob published under the identity-wide Paykit namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaykitBlobRecord {
    /// Blob owner.
    pub public_key: PubkyPublicKey,
    /// Pubky path used for the blob.
    pub path: String,
    /// Canonical `pubky://` URI for the blob.
    pub uri: String,
    /// Blob size in bytes.
    pub size_bytes: u64,
    /// Local publication time.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

fn validate_paykit_blob_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(PaykitSdkError::Protocol {
            context: "Paykit blob name must not be empty".into(),
            source: None,
        });
    }
    if name.len() > MAX_PAYKIT_BLOB_NAME_BYTES {
        return Err(PaykitSdkError::Protocol {
            context: format!("Paykit blob name must not exceed {MAX_PAYKIT_BLOB_NAME_BYTES} bytes"),
            source: None,
        });
    }
    if name == "." || name == ".." {
        return Err(PaykitSdkError::Protocol {
            context: "Paykit blob name must not be a path traversal segment".into(),
            source: None,
        });
    }
    if name
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_'))
    {
        return Err(PaykitSdkError::Protocol {
            context: "Paykit blob name may only contain ASCII letters, digits, '.', '-' and '_'"
                .into(),
            source: None,
        });
    }
    Ok(())
}

pub(crate) fn paykit_blob_path(blob_prefix: &str, name: &str) -> Result<String> {
    validate_paykit_blob_name(name)?;
    Ok(format!("{blob_prefix}{name}"))
}

pub(crate) fn paykit_blob_uri(public_key: &PubkyPublicKey, path: &str) -> String {
    format!("pubky://{}{}", public_key.as_str(), path)
}

pub(crate) fn paykit_blob_path_from_uri_or_path(
    public_key: &PubkyPublicKey,
    blob_prefix: &str,
    uri_or_path: &str,
) -> Result<String> {
    if uri_or_path.starts_with("pubky://") || uri_or_path.starts_with("pubky") {
        let resource = uri_or_path.parse::<pubky::PubkyResource>().map_err(|err| {
            PaykitSdkError::Protocol {
                context: format!("invalid Pubky blob URI: {err}"),
                source: None,
            }
        })?;
        let owner = PubkyPublicKey::from_public_key(&resource.owner);
        if &owner != public_key {
            return Err(PaykitSdkError::Protocol {
                context: "Paykit blob URI owner does not match local identity".into(),
                source: None,
            });
        }
        return validate_paykit_blob_path(blob_prefix, resource.path.as_str());
    }
    validate_paykit_blob_path(blob_prefix, uri_or_path)
}

fn validate_paykit_blob_path(blob_prefix: &str, path: &str) -> Result<String> {
    let name = path
        .strip_prefix(blob_prefix)
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: "Paykit blob path is outside the Paykit blob prefix".into(),
            source: None,
        })?;
    validate_paykit_blob_name(name)?;
    Ok(path.to_owned())
}
