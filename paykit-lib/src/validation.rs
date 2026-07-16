use crate::{PaykitError, PrivateMessageKind, Result};

pub(crate) fn validate_uuid_v4(value: String, label: &'static str) -> Result<String> {
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

pub(crate) fn invalid_data(
    context: impl Into<String>,
    source: Option<anyhow::Error>,
) -> PaykitError {
    PaykitError::InvalidData {
        context: context.into(),
        source,
    }
}

/// Build the `InvalidData` error for JSON that was decrypted from private
/// plaintext (an Encrypted Receipt payload or a private message body).
///
/// SECURITY / REDACTION: serde_json's error `Display` embeds verbatim document
/// fragments on type mismatches (e.g. `invalid type: string "<field value>"`).
/// The document here is decrypted plaintext, so the parse error must reach no
/// sink: the context stays a static label and the serde cause is deliberately
/// dropped (no `source`), keeping plaintext out of error chains, logs, and the
/// FFI-facing strings derived from this error.
pub(crate) fn invalid_plaintext_json(context: &'static str) -> PaykitError {
    PaykitError::InvalidData {
        context: context.into(),
        source: None,
    }
}

pub(crate) fn invalid_wire(err: PaykitError, label: &'static str) -> PaykitError {
    match err {
        PaykitError::Validation(msg) => invalid_data(format!("{label}: {msg}"), None),
        other => other,
    }
}

pub(crate) fn parse_utc_timestamp(
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

pub(crate) fn validate_wire_version_kind(
    version: u8,
    kind: &str,
    expected: PrivateMessageKind,
    label: &'static str,
) -> Result<()> {
    validate_wire_version_kind_str(version, kind, expected.as_str(), label)
}

pub(crate) fn validate_wire_version_kind_str(
    version: u8,
    kind: &str,
    expected_kind: &str,
    label: &'static str,
) -> Result<()> {
    if version != 1 || kind != expected_kind {
        // SECURITY / REDACTION: every caller validates decrypted
        // private-message plaintext, and this context can cross the FFI
        // boundary as exception text. The offending `version`/`kind` values
        // are decrypted field values and must not be echoed; only the static
        // label may cross.
        return Err(invalid_data(
            format!("unsupported {label} version/kind"),
            None,
        ));
    }
    Ok(())
}

pub(crate) fn validate_outgoing_version_kind(
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
