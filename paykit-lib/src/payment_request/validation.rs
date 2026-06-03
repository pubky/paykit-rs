use crate::{PaykitError, PrivateMessageKind, Result};

pub(super) fn validate_uuid_v4(value: String, label: &'static str) -> Result<String> {
    let uuid = uuid::Uuid::try_parse(&value).map_err(|err| {
        PaykitError::Validation(format!("{label} must be a UUID v4 string: {err}"))
    })?;
    if uuid.get_version_num() != 4 || uuid.get_variant() != uuid::Variant::RFC4122 {
        return Err(PaykitError::Validation(format!(
            "{label} must be an RFC4122 UUID v4 string"
        )));
    }
    Ok(uuid.hyphenated().to_string())
}

pub(super) fn invalid_data(
    context: impl Into<String>,
    source: Option<anyhow::Error>,
) -> PaykitError {
    PaykitError::InvalidData {
        context: context.into(),
        source,
    }
}

pub(super) fn invalid_wire(err: PaykitError, label: &'static str) -> PaykitError {
    match err {
        PaykitError::Validation(msg) => invalid_data(format!("{label}: {msg}"), None),
        other => other,
    }
}

pub(super) fn parse_utc_timestamp(
    value: &str,
    field: &str,
) -> Result<chrono::DateTime<chrono::FixedOffset>> {
    if !value.ends_with('Z') {
        return Err(PaykitError::Validation(format!(
            "{field} must be an RFC3339 UTC timestamp using the Z suffix"
        )));
    }
    chrono::DateTime::parse_from_rfc3339(value).map_err(|err| {
        PaykitError::Validation(format!("{field} must be a valid RFC3339 timestamp: {err}"))
    })
}

pub(super) fn validate_version_kind(
    version: u8,
    kind: &str,
    expected: PrivateMessageKind,
) -> Result<()> {
    if version != 1 || kind != expected.as_str() {
        return Err(PaykitError::InvalidData {
            context: format!("unsupported Payment Request event version/kind: {version}/{kind}"),
            source: None,
        });
    }
    Ok(())
}

pub(super) fn validate_outgoing_version_kind(
    version: u8,
    kind: PrivateMessageKind,
    expected: PrivateMessageKind,
    label: &'static str,
) -> Result<()> {
    if version != 1 || kind != expected {
        return Err(PaykitError::Validation(format!(
            "{label} must use version 1 and kind {}",
            expected.as_str()
        )));
    }
    Ok(())
}
