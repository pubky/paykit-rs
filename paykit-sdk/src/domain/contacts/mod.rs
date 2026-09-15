//! Contact and profile types.

mod blobs;
mod contact_records;
mod profiles;

pub use blobs::PaykitBlobRecord;
pub use contact_records::{ContactRecord, ContactUpdate};
pub use profiles::{
    PaykitProfile, PaykitProfileRecord, ProfileResolution, ProfileSource, PubkyProfile,
    PubkyProfileLink, PubkyProfileRecord, PUBKY_FOLLOWS_PATH_PREFIX, PUBKY_PROFILE_PATH,
};

pub(crate) use blobs::{
    paykit_blob_path, paykit_blob_path_from_uri_or_path, paykit_blob_uri,
    PAYKIT_PROFILE_BLOB_PATH_PREFIX,
};
pub(crate) use contact_records::{public_contact_json, public_contact_path};
pub(crate) use profiles::{
    parse_profile_json, parse_pubky_profile_json, profile_json,
    pubky_follow_keys_from_follow_entries, PAYKIT_PROFILE_PATH,
};

use crate::{PaykitSdkError, Result};

fn validate_optional_text(
    value: Option<&str>,
    label: &str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if !allow_empty && value.trim().is_empty() {
        return Err(PaykitSdkError::Protocol {
            context: format!("{label} must not be empty"),
            source: None,
        });
    }
    if value.chars().count() > max_chars {
        return Err(PaykitSdkError::Protocol {
            context: format!("{label} must not exceed {max_chars} characters"),
            source: None,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(PaykitSdkError::Protocol {
            context: format!("{label} must not contain control characters"),
            source: None,
        });
    }
    Ok(())
}

fn validate_text(value: &str, label: &str, max_chars: usize, allow_empty: bool) -> Result<()> {
    validate_optional_text(Some(value), label, max_chars, allow_empty)
}

#[cfg(test)]
mod tests;
