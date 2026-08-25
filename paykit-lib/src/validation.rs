use crate::{
    PaykitError, PrivateMessageKind, PrivateMessageParseCategory, PrivateMessageParseError, Result,
};

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

/// Build the redacted `InvalidData` error for a private-message parse failure.
///
/// SECURITY / REDACTION: the context must be a static label with no data
/// interpolation, because these errors describe decrypted private-message
/// plaintext and can cross the FFI boundary as exception text. The only
/// machine-readable detail is the typed [`PrivateMessageParseError`] source,
/// recoverable via [`PaykitError::private_message_parse_category`].
pub(crate) fn private_message_parse_error(
    context: &'static str,
    category: PrivateMessageParseCategory,
) -> PaykitError {
    private_message_parse_error_with_context(context.to_owned(), category)
}

/// String-context variant of [`private_message_parse_error`] for call sites
/// whose context interpolates a static label (never data values).
pub(crate) fn private_message_parse_error_with_context(
    context: String,
    category: PrivateMessageParseCategory,
) -> PaykitError {
    PaykitError::InvalidData {
        context,
        source: Some(anyhow::Error::new(PrivateMessageParseError::new(category))),
    }
}

/// Classify a serde_json error into a redacted parse category without
/// retaining the error itself.
///
/// `Data` errors mean the document was syntactically valid JSON that failed
/// structural expectations; `Syntax`, `Eof`, and `Io` mean the document was
/// not valid JSON at all.
pub(crate) fn json_error_category(err: &serde_json::Error) -> PrivateMessageParseCategory {
    match err.classify() {
        serde_json::error::Category::Data => PrivateMessageParseCategory::InvalidStructure,
        serde_json::error::Category::Syntax
        | serde_json::error::Category::Eof
        | serde_json::error::Category::Io => PrivateMessageParseCategory::InvalidJson,
    }
}

/// Remap a structural `Validation` failure on PUBLIC wire data (e.g. an
/// Encrypted Link recovery marker fetched from public Pubky storage) into
/// `InvalidData`, preserving the validator's diagnostic message.
///
/// Public wire data is not decrypted private-message plaintext, so keeping
/// the inner message is safe and useful, and no redacted parse category is
/// attached: [`crate::PaykitError::private_message_parse_category`] must
/// return `None` for these errors. Parsers of decrypted private-message
/// plaintext use [`invalid_private_wire`] instead.
pub(crate) fn invalid_wire(err: PaykitError, label: &'static str) -> PaykitError {
    match err {
        PaykitError::Validation(msg) => invalid_data(format!("{label}: {msg}"), None),
        other => other,
    }
}

/// Redacting variant of [`invalid_wire`] for decrypted private-message
/// plaintext.
///
/// SECURITY / REDACTION: the dropped Validation message can embed decrypted
/// field values (identifier, timestamp, and UUID validators echo the
/// offending value). Only the static label survives; the typed
/// InvalidStructure source keeps the failure machine-readable.
pub(crate) fn invalid_private_wire(err: PaykitError, label: &'static str) -> PaykitError {
    match err {
        PaykitError::Validation(_) => private_message_parse_error_with_context(
            format!("{label} failed structural validation"),
            PrivateMessageParseCategory::InvalidStructure,
        ),
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
    // SECURITY / REDACTION: every caller validates decrypted private-message
    // plaintext, and these contexts can cross the FFI boundary as exception
    // text. The offending `version`/`kind` values are decrypted field values
    // and must not be echoed; only the static label may cross. The two checks
    // are separate so the typed category distinguishes them.
    if version != 1 {
        return Err(private_message_parse_error_with_context(
            format!("unsupported {label} version"),
            PrivateMessageParseCategory::UnsupportedVersion,
        ));
    }
    if kind != expected_kind {
        return Err(private_message_parse_error_with_context(
            format!("unsupported {label} kind"),
            PrivateMessageParseCategory::WrongKind,
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
