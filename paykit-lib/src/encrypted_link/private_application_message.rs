use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pubky::{errors::RequestError, Error as PubkyError, PubkySession, PublicKey, StatusCode};
use tracing::{debug, warn};

use crate::{
    encrypted_link::paths::compute_private_payment_paths,
    error::{NonRetryablePrivateReceiveError, NonRetryablePrivateSendError},
    PaykitError, PaykitReceiverPath, Result,
};

// Local marker used when decrypted private plaintext is not valid UTF-8.
// This is not a protocol message. It lets receive callers persist the malformed
// stream item, advance their local link checkpoint, and avoid wedging on the
// same encrypted slot forever. Visible to the sibling `inspection` module so
// the shared inspection entry point can special-case persisted marker payloads
// without widening the marker's visibility beyond the encrypted_link tree.
pub(super) const INVALID_UTF8_PRIVATE_MESSAGE_PREFIX: &str = "paykit.invalid_utf8_private_message:";
const LIST_PAGE_LIMIT: u16 = 100;

/// Private Message Kind values understood by Paykit.
///
/// This enum is intentionally exhaustive. Adding a variant must produce
/// compile-time failures in routing, validation, and backup-validation matches
/// until the new Private Message Kind is classified explicitly. Do not add
/// `#[non_exhaustive]` without a coordinated team decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateMessageKind {
    /// Private Payment List Latest-State Message (`paykit.private_payment_list`).
    PrivatePaymentList,
    /// Receipt Access Event Message (`paykit.receipt_access`).
    ReceiptAccess,
    /// Payment Request Event Message (`paykit.payment_request`).
    PaymentRequest,
    /// Payment Request Acceptance Event Message (`paykit.payment_request_acceptance`).
    PaymentRequestAcceptance,
    /// Payment Request Rejection Event Message (`paykit.payment_request_rejection`).
    PaymentRequestRejection,
    /// Payment Request Cancellation Event Message (`paykit.payment_request_cancellation`).
    PaymentRequestCancellation,
    /// Payment Proof Event Message (`paykit.payment_proof`).
    PaymentProof,
}

impl PrivateMessageKind {
    /// Every Private Message Kind, in canonical declaration order.
    ///
    /// [`PrivateMessageKind::parse`] is a lookup over this table, so a new
    /// variant is only parseable once listed here. The table's length is
    /// derived from the private `LAST` pin next to `variant_index`, so once
    /// that pin is re-pointed at a new variant this table fails to compile
    /// until the variant is appended; the const guard below this impl asserts
    /// every listed entry sits at its declaration-order index.
    pub const ALL: [Self; Self::COUNT] = [
        Self::PrivatePaymentList,
        Self::ReceiptAccess,
        Self::PaymentRequest,
        Self::PaymentRequestAcceptance,
        Self::PaymentRequestRejection,
        Self::PaymentRequestCancellation,
        Self::PaymentProof,
    ];

    /// The final declared variant, pinned by name.
    ///
    /// This pin is the one completeness edit the compiler cannot force: when
    /// adding a variant, re-point it at the new last variant. The derived
    /// [`PrivateMessageKind::ALL`] length and the const guard below this impl
    /// then reject any table that does not cover the enum in declaration
    /// order.
    const LAST: Self = Self::PaymentProof;

    /// Number of declared variants, derived from the `LAST` pin.
    const COUNT: usize = Self::LAST.variant_index() + 1;

    /// Declaration-order index of this variant, used only by the `LAST` and
    /// `COUNT` pins and the const completeness guard below this impl.
    ///
    /// When adding a variant: give it the next index here and re-point `LAST`
    /// at it; [`PrivateMessageKind::ALL`] then fails to compile until the
    /// variant is appended, and the exhaustive matches in
    /// [`PrivateMessageKind::as_str`], `semantics`, and
    /// `is_payment_request_event` force their own arms.
    const fn variant_index(self) -> usize {
        match self {
            Self::PrivatePaymentList => 0,
            Self::ReceiptAccess => 1,
            Self::PaymentRequest => 2,
            Self::PaymentRequestAcceptance => 3,
            Self::PaymentRequestRejection => 4,
            Self::PaymentRequestCancellation => 5,
            Self::PaymentProof => 6,
        }
    }

    /// Return the canonical private message kind string used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrivatePaymentList => "paykit.private_payment_list",
            Self::ReceiptAccess => "paykit.receipt_access",
            Self::PaymentRequest => "paykit.payment_request",
            Self::PaymentRequestAcceptance => "paykit.payment_request_acceptance",
            Self::PaymentRequestRejection => "paykit.payment_request_rejection",
            Self::PaymentRequestCancellation => "paykit.payment_request_cancellation",
            Self::PaymentProof => "paykit.payment_proof",
        }
    }

    /// Parse a canonical private message kind string.
    ///
    /// Implemented as a lookup over [`PrivateMessageKind::ALL`] so a new
    /// variant is parseable as soon as it is listed there; there is no
    /// wildcard arm to defeat exhaustiveness.
    pub fn parse(kind: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == kind)
    }

    /// Return the message-processing semantics for this Private Message Kind.
    ///
    /// The match is deliberately exhaustive with no wildcard: adding a
    /// variant fails to compile until its Latest-State Message versus Event
    /// Message semantics are chosen explicitly here.
    pub fn semantics(self) -> PrivateMessageSemantics {
        match self {
            Self::PrivatePaymentList => PrivateMessageSemantics::LatestState,
            Self::ReceiptAccess => PrivateMessageSemantics::Event,
            Self::PaymentRequest => PrivateMessageSemantics::Event,
            Self::PaymentRequestAcceptance => PrivateMessageSemantics::Event,
            Self::PaymentRequestRejection => PrivateMessageSemantics::Event,
            Self::PaymentRequestCancellation => PrivateMessageSemantics::Event,
            Self::PaymentProof => PrivateMessageSemantics::Event,
        }
    }

    /// Whether this kind is a Payment Request protocol Event Message kind,
    /// i.e. one [`crate::parse_payment_request_event_message`] parses.
    ///
    /// This mirrors the wildcard-free Payment Request event routing in
    /// `payment_request::api::parse_event`: the five Payment Request
    /// lifecycle kinds route to a parser, while Private Payment List and
    /// Receipt Access do not. The match is deliberately exhaustive so adding
    /// a variant fails to compile until it is classified explicitly here as
    /// well.
    pub fn is_payment_request_event(self) -> bool {
        match self {
            Self::PaymentRequest
            | Self::PaymentRequestAcceptance
            | Self::PaymentRequestRejection
            | Self::PaymentRequestCancellation
            | Self::PaymentProof => true,
            Self::PrivatePaymentList | Self::ReceiptAccess => false,
        }
    }
}

// Const completeness guard for `PrivateMessageKind::ALL`: every `ALL` entry
// must sit at its declaration-order index, and the table's length is derived
// from the `LAST` pin, so `ALL` holds exactly the declared variants in order,
// provided `LAST` names the enum's true final variant. The guard is only
// as strong as that pin: re-pointing `LAST` when adding a variant is a
// documented manual step at the `variant_index` compile error, not something
// the compiler can force.
const _: () = {
    let mut index = 0;
    while index < PrivateMessageKind::ALL.len() {
        assert!(
            PrivateMessageKind::ALL[index].variant_index() == index,
            "PrivateMessageKind::ALL must list every variant in declaration order"
        );
        index += 1;
    }
};

impl std::fmt::Display for PrivateMessageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Message-processing semantics of a recognized Private Message Kind.
///
/// The vocabulary follows `THESAURUS.md`: a Latest-State Message is one FIFO
/// private Paykit message where the latest valid message of a kind supersedes
/// older messages of the same kind, while an Event Message is one where every
/// valid message matters and receivers must process messages in send order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateMessageSemantics {
    /// Latest-State Message semantics: newer valid messages of this kind
    /// supersede older ones (the Private Payment List).
    LatestState,
    /// Event Message semantics: every valid message matters, is processed in
    /// send order, and carries an Event ID (Receipt Access and the Payment
    /// Request lifecycle kinds).
    Event,
}

/// Stable redacted category for a private-message parse failure.
///
/// COMPATIBILITY CONTRACT: the strings returned by
/// [`PrivateMessageParseCategory::as_str`] are persisted in durable SDK state
/// (stream-item `parse_error` fields) and byte-compared during backup restore.
/// They are a permanent compatibility contract: never change an existing
/// string once it has shipped. New failure modes get new variants with new
/// strings, appended to [`PrivateMessageParseCategory::ALL`].
///
/// SECURITY / REDACTION: these categories exist so parse errors for decrypted
/// private-message plaintext carry no serde detail. serde error text embeds
/// verbatim document fragments on type mismatches, and these errors cross the
/// FFI boundary as exception text, so the category string is the only
/// diagnostic that may leave the parse site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateMessageParseCategory {
    /// The plaintext is not syntactically valid JSON.
    InvalidJson,
    /// The message declares a private message version this library does not
    /// support.
    UnsupportedVersion,
    /// The message declares a kind the invoked parser does not accept.
    WrongKind,
    /// The plaintext is valid JSON but fails structural validation for its
    /// recognized kind.
    InvalidStructure,
    /// The decrypted plaintext is not valid UTF-8 (the local receive marker).
    InvalidUtf8Plaintext,
}

impl PrivateMessageParseCategory {
    /// Every parse category, in declaration order.
    ///
    /// [`PrivateMessageParseCategory::parse`] is a lookup over this table, so
    /// a new variant is only parseable once listed here. Adding a variant is
    /// a compile error at the exhaustive private `variant_index` match, whose
    /// instructions walk through extending this table; the const guard below
    /// this impl asserts every listed entry sits at its declared index and
    /// that the table ends at the guard's named last variant.
    pub const ALL: [Self; 5] = [
        Self::InvalidJson,
        Self::UnsupportedVersion,
        Self::WrongKind,
        Self::InvalidStructure,
        Self::InvalidUtf8Plaintext,
    ];

    /// Declaration-order index of this variant, used only by the const
    /// completeness guard below this impl.
    ///
    /// When adding a variant: give it the next index here, append it to
    /// [`PrivateMessageParseCategory::ALL`] (with a NEW string in `as_str`;
    /// existing strings are frozen), and point the guard's final-slot
    /// assertion at the new last variant.
    const fn variant_index(self) -> usize {
        match self {
            Self::InvalidJson => 0,
            Self::UnsupportedVersion => 1,
            Self::WrongKind => 2,
            Self::InvalidStructure => 3,
            Self::InvalidUtf8Plaintext => 4,
        }
    }

    /// Return the canonical persisted string for this category.
    ///
    /// These strings are byte-compared on backup restore and must never
    /// change (see the type-level compatibility contract).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid private message JSON",
            Self::UnsupportedVersion => "unsupported private message version",
            Self::WrongKind => "unsupported private message kind",
            Self::InvalidStructure => "invalid private message structure",
            Self::InvalidUtf8Plaintext => {
                "Private Application Message plaintext is not valid UTF-8"
            }
        }
    }

    /// Parse a canonical persisted category string.
    ///
    /// Implemented as a lookup over [`PrivateMessageParseCategory::ALL`] so a
    /// new variant is parseable as soon as it is listed there; there is no
    /// wildcard arm to defeat exhaustiveness.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|category| category.as_str() == value)
    }
}

// Const completeness guard for `PrivateMessageParseCategory::ALL` (a
// permanent compatibility contract): every `ALL` entry must sit at its
// declaration-order index, and the final declared variant must occupy the
// final slot, so `ALL` cannot silently fall out of sync with the enum.
const _: () = {
    let mut index = 0;
    while index < PrivateMessageParseCategory::ALL.len() {
        assert!(
            PrivateMessageParseCategory::ALL[index].variant_index() == index,
            "PrivateMessageParseCategory::ALL must list every variant in declaration order"
        );
        index += 1;
    }
    assert!(
        PrivateMessageParseCategory::ALL.len()
            == PrivateMessageParseCategory::InvalidUtf8Plaintext.variant_index() + 1,
        "PrivateMessageParseCategory::ALL must end at the last declared variant"
    );
};

impl std::fmt::Display for PrivateMessageParseCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed source attached to redacted private-message parse errors.
///
/// Carries only a [`PrivateMessageParseCategory`]; it never holds serde detail
/// or plaintext fragments, so it is safe in error chains, logs, and
/// FFI-facing strings. Recover it from a [`crate::PaykitError`] via
/// [`crate::PaykitError::private_message_parse_category`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}", .category.as_str())]
pub struct PrivateMessageParseError {
    category: PrivateMessageParseCategory,
}

impl PrivateMessageParseError {
    /// Wrap a parse category as a typed error source.
    pub fn new(category: PrivateMessageParseCategory) -> Self {
        Self { category }
    }

    /// The redacted parse category this error carries.
    pub fn category(&self) -> PrivateMessageParseCategory {
        self.category
    }
}

/// One Private Application Message received from an Encrypted Link.
///
/// This is the low-level receive item for the Private Application Message stream.
/// It keeps the raw plaintext payload so callers can route and persist messages
/// themselves. If decrypted bytes are not UTF-8, `raw_json` contains a local
/// invalid-JSON marker with the bytes encoded as base64url. Higher layers should
/// treat that marker as malformed input, not as a Private Application Message.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateApplicationMessage {
    /// Private Application Message version from the JSON `version` field, when
    /// the field is present and can be represented as a `u8`.
    pub version: Option<u8>,
    /// Message kind string from the JSON `kind` field, when the field is a
    /// string.
    pub kind: Option<String>,
    /// Raw plaintext received over the Encrypted Link.
    pub raw_json: String,
}

impl std::fmt::Debug for PrivateApplicationMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateApplicationMessage")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field(
                "raw_json",
                &format_args!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .finish()
    }
}

impl PrivateApplicationMessage {
    /// Build a Private Application Message from decrypted plaintext, deriving
    /// the envelope header fields.
    ///
    /// `version` and `kind` are best-effort reads of the top-level JSON
    /// fields: a non-JSON payload, an absent field, or an out-of-range
    /// version becomes `None`. This is the single header-derivation code
    /// path: the receive decoder, [`crate::inspect_private_application_message`],
    /// and SDK header re-derivation over persisted `raw_json` all construct
    /// messages here, so header semantics cannot drift between them. Callers
    /// that only need header fields use this directly instead of running the
    /// body parsers behind the full inspection entry point.
    pub fn from_plaintext(plaintext: String) -> Self {
        let value = serde_json::from_str::<serde_json::Value>(&plaintext).ok();
        let version = value
            .as_ref()
            .and_then(|value| value.get("version"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u8::try_from(version).ok());
        let kind = value
            .as_ref()
            .and_then(|value| value.get("kind"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Self {
            version,
            kind,
            raw_json: plaintext,
        }
    }

    /// Return the known Paykit kind from the raw payload, if this library can
    /// parse and recognize it.
    pub fn known_kind(&self) -> Option<PrivateMessageKind> {
        serde_json::from_str::<serde_json::Value>(&self.raw_json)
            .ok()
            .and_then(|value| {
                value
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .and_then(PrivateMessageKind::parse)
            })
    }

    /// Return a parse error for the local invalid-UTF-8 receive marker.
    ///
    /// SDK/runtime callers use this to persist an audit/error record for a
    /// malformed stream item while still advancing the local Encrypted Link
    /// checkpoint. This is not expected on valid protocol messages.
    pub fn invalid_utf8_error(&self) -> Option<&'static str> {
        // The returned string is the persisted parse-error text for this case
        // and is byte-compared on backup restore; sourcing it from the parse
        // category keeps the two identical by construction.
        self.raw_json
            .starts_with(INVALID_UTF8_PRIVATE_MESSAGE_PREFIX)
            .then_some(PrivateMessageParseCategory::InvalidUtf8Plaintext.as_str())
    }
}

fn decode_private_application_message(
    raw: &[u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN],
) -> Result<PrivateApplicationMessage> {
    // Trim trailing zero-padding added by pubky-noise's fixed-size buffers.
    // Paykit application messages are JSON, so trailing NUL bytes are not valid
    // payload content.
    let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let plaintext = match std::str::from_utf8(&raw[..end]) {
        Ok(plaintext) => plaintext.to_owned(),
        Err(_) => format!(
            "{INVALID_UTF8_PRIVATE_MESSAGE_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(&raw[..end])
        ),
    };

    Ok(PrivateApplicationMessage::from_plaintext(plaintext))
}

pub(super) async fn receive_private_application_messages(
    encryptor: &mut pubky_noise::PubkyNoiseEncryptor,
) -> Result<Vec<PrivateApplicationMessage>> {
    let mut messages = Vec::new();

    loop {
        let batch = encryptor.receive_message().await.map_err(|err| {
            let context = format!("failed to receive Private Application Messages: {err:?}");
            let source = if is_retryable_private_receive_error(&err) {
                anyhow::anyhow!("pubky-noise receive_message failed: {err:?}")
            } else {
                anyhow::Error::new(NonRetryablePrivateReceiveError(err))
            };
            PaykitError::Transport { context, source }
        })?;

        if batch.is_empty() {
            break;
        }

        for raw in batch {
            messages.push(decode_private_application_message(&raw)?);
        }
    }

    Ok(messages)
}

/// Delete all encrypted stream slots written by the local identity for one
/// counterparty.
///
/// This clears the local write path used by the counterparty as their read path.
/// It is intended for recovery before starting a fresh Encrypted Link Handshake
/// after the previous link state has been abandoned. The counterparty's outbox
/// is not touched.
pub async fn clear_encrypted_link_outbox(
    session: &PubkySession,
    local_noise_secret_key: &[u8; 32],
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
) -> Result<usize> {
    let local_identity_public_key = session.info().public_key();
    let (write_path, _) = compute_private_payment_paths(
        local_noise_secret_key,
        local_identity_public_key,
        remote_identity_public_key,
        remote_noise_public_key,
        local_receiver_path,
        remote_receiver_path,
    );
    let list_path = format!("{write_path}/");
    let storage = session.storage();
    let mut deleted_count = 0;

    loop {
        let builder = match storage.list(&list_path) {
            Ok(builder) => builder.shallow(true).limit(LIST_PAGE_LIMIT),
            Err(err) if is_not_found(&err) => return Ok(deleted_count),
            Err(err) => {
                return Err(PaykitError::Transport {
                    context: "list Encrypted Link outbox".into(),
                    source: err.into(),
                });
            }
        };

        let page = match builder.send().await {
            Ok(page) => page,
            Err(err) if is_not_found(&err) => return Ok(deleted_count),
            Err(err) => {
                return Err(PaykitError::Transport {
                    context: "list Encrypted Link outbox".into(),
                    source: err.into(),
                });
            }
        };

        if page.is_empty() {
            return Ok(deleted_count);
        }

        let page_len = page.len();
        for resource in page {
            match storage.delete(resource.path.as_str()).await {
                Ok(_) => deleted_count += 1,
                Err(err) if is_not_found(&err) => {}
                Err(err) => {
                    return Err(PaykitError::Transport {
                        context: "clear Encrypted Link outbox".into(),
                        source: err.into(),
                    });
                }
            }
        }

        if page_len < LIST_PAGE_LIMIT as usize {
            return Ok(deleted_count);
        }
    }
}

fn send_attempts_from_retries(max_send_retries: u32) -> u32 {
    max_send_retries.saturating_add(1)
}

fn is_retryable_private_send_error(err: &pubky_noise::PubkyNoiseError) -> bool {
    matches!(err, pubky_noise::PubkyNoiseError::HomeserverWriteError)
}

fn is_retryable_private_receive_error(err: &pubky_noise::PubkyNoiseError) -> bool {
    matches!(err, pubky_noise::PubkyNoiseError::HomeserverResponseError)
}

fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}

fn validate_private_application_message_size(
    plaintext: &[u8],
    context: &'static str,
) -> Result<()> {
    if plaintext.len() > pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN {
        return Err(PaykitError::Validation(format!(
            "{context} payload ({} bytes) exceeds max message size ({} bytes)",
            plaintext.len(),
            pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
        )));
    }
    Ok(())
}

pub(super) async fn send_private_application_message(
    encryptor: &mut pubky_noise::PubkyNoiseEncryptor,
    max_send_retries: u32,
    plaintext: &[u8],
    context: &'static str,
) -> Result<()> {
    validate_private_application_message_size(plaintext, context)?;

    let max_attempts = send_attempts_from_retries(max_send_retries);
    let mut last_error: Option<String> = None;

    for attempt in 1..=max_attempts {
        match encryptor.send_message(plaintext).await {
            Ok(()) => {
                debug!(context, "Private Application Message sent successfully");
                return Ok(());
            }
            Err(err) if is_retryable_private_send_error(&err) => {
                last_error = Some(format!("{err:?}"));
                if attempt < max_attempts {
                    warn!(
                        attempt,
                        max_retries = max_send_retries,
                        error = ?err,
                        context,
                        "send_message failed, retrying"
                    );
                }
            }
            Err(err) => {
                let context = format!("failed to send {context}: {err:?}");
                return Err(PaykitError::Transport {
                    context,
                    source: anyhow::Error::new(NonRetryablePrivateSendError(err)),
                });
            }
        }
    }

    Err(PaykitError::Transport {
        context: format!("failed to send {context} after {max_attempts} attempts"),
        source: anyhow::anyhow!(
            "pubky-noise send_message failed on all {} attempts; last error: {}",
            max_attempts,
            last_error.unwrap_or_else(|| "unknown error".to_string())
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        serialize_private_payment_list_json, PaymentEndpointIdentifier, PaymentEndpointPayload,
        PrivatePaymentList,
    };
    use std::collections::HashMap;

    #[test]
    fn test_private_message_kind_parse_round_trips_all() {
        for kind in PrivateMessageKind::ALL {
            assert_eq!(
                PrivateMessageKind::parse(kind.as_str()),
                Some(kind),
                "kind {kind:?} must round-trip through its canonical string"
            );
        }
        for near_miss in [
            "",
            "paykit",
            "paykit.",
            "payment_request",
            "paykit.allowance",
            "paykit.private_payment_lists",
            " paykit.private_payment_list",
            "paykit.private_payment_list ",
            "paykit.Payment_Request",
            "paykit.payment-request",
            "paykit.payment_request2",
            "paykit.receipt_access\0",
        ] {
            assert_eq!(
                PrivateMessageKind::parse(near_miss),
                None,
                "near-miss string {near_miss:?} must not parse as a kind"
            );
        }
    }

    #[test]
    fn test_semantics_assignments() {
        // Latest-State Message semantics apply only to the Private Payment
        // List; every other kind is an Event Message. Iterating ALL keeps this
        // pin complete when a variant is added.
        for kind in PrivateMessageKind::ALL {
            let expected = if kind == PrivateMessageKind::PrivatePaymentList {
                PrivateMessageSemantics::LatestState
            } else {
                PrivateMessageSemantics::Event
            };
            assert_eq!(
                kind.semantics(),
                expected,
                "kind {kind:?} has unexpected semantics"
            );
        }
    }

    #[test]
    fn test_is_payment_request_event_matches_parse_event_routing() {
        // Cross-check the predicate against the actual Payment Request event
        // routing: `parse_payment_request_event_message` returns `Some`
        // exactly for the kinds its internal `parse_event` routes to a
        // parser, so the predicate and the dispatch cannot drift apart.
        for kind in PrivateMessageKind::ALL {
            let raw = format!(r#"{{"version":1,"kind":"{}"}}"#, kind.as_str());
            let message = PrivateApplicationMessage::from_plaintext(raw);
            assert_eq!(
                crate::parse_payment_request_event_message(&message).is_some(),
                kind.is_payment_request_event(),
                "kind {kind:?} predicate disagrees with parse_event routing"
            );
        }
    }

    #[test]
    fn test_private_message_parse_category_round_trips_through_all() {
        for category in PrivateMessageParseCategory::ALL {
            assert_eq!(
                PrivateMessageParseCategory::parse(category.as_str()),
                Some(category),
                "category {category:?} must round-trip through its string"
            );
        }
        assert_eq!(PrivateMessageParseCategory::parse("unrecognized"), None);
        assert_eq!(PrivateMessageParseCategory::parse(""), None);
    }

    #[test]
    fn test_private_message_parse_category_strings_are_frozen() {
        // COMPATIBILITY CONTRACT: these strings are persisted in durable SDK
        // state and byte-compared on backup restore. This test failing means a
        // string changed; that breaks restore of existing backups and must
        // never happen. Add new variants with new strings instead.
        let expected = [
            (
                PrivateMessageParseCategory::InvalidJson,
                "invalid private message JSON",
            ),
            (
                PrivateMessageParseCategory::UnsupportedVersion,
                "unsupported private message version",
            ),
            (
                PrivateMessageParseCategory::WrongKind,
                "unsupported private message kind",
            ),
            (
                PrivateMessageParseCategory::InvalidStructure,
                "invalid private message structure",
            ),
            (
                PrivateMessageParseCategory::InvalidUtf8Plaintext,
                "Private Application Message plaintext is not valid UTF-8",
            ),
        ];
        assert_eq!(expected.len(), PrivateMessageParseCategory::ALL.len());
        for (category, string) in expected {
            assert_eq!(category.as_str(), string);
        }
    }

    #[test]
    fn test_private_message_parse_error_exposes_category_through_paykit_error() {
        let err = PaykitError::InvalidData {
            context: "failed to parse Private Payment List JSON".into(),
            source: Some(anyhow::Error::new(PrivateMessageParseError::new(
                PrivateMessageParseCategory::InvalidJson,
            ))),
        };
        assert_eq!(
            err.private_message_parse_category(),
            Some(PrivateMessageParseCategory::InvalidJson)
        );
        // Display of the typed source is exactly the category string.
        assert_eq!(
            PrivateMessageParseError::new(PrivateMessageParseCategory::WrongKind).to_string(),
            PrivateMessageParseCategory::WrongKind.as_str()
        );

        let untyped = PaykitError::InvalidData {
            context: "no typed source".into(),
            source: Some(anyhow::anyhow!("plain source")),
        };
        assert_eq!(untyped.private_message_parse_category(), None);
        assert_eq!(
            PaykitError::Validation("caller input".into()).private_message_parse_category(),
            None
        );
    }

    #[test]
    fn test_send_attempts_from_retries_bounds() {
        assert_eq!(send_attempts_from_retries(0), 1);
        assert_eq!(send_attempts_from_retries(3), 4);
        assert_eq!(send_attempts_from_retries(u32::MAX), u32::MAX);
    }

    #[test]
    fn test_private_send_retry_classification() {
        assert!(is_retryable_private_send_error(
            &pubky_noise::PubkyNoiseError::HomeserverWriteError,
        ));

        for err in [
            pubky_noise::PubkyNoiseError::IsHandshake,
            pubky_noise::PubkyNoiseError::EncryptionError,
            pubky_noise::PubkyNoiseError::CounterOverflow,
            pubky_noise::PubkyNoiseError::NonceOverflow,
        ] {
            assert!(
                !is_retryable_private_send_error(&err),
                "{err:?} should not be retried"
            );
        }
    }

    #[test]
    fn test_non_retryable_private_send_error_is_classified() {
        let err = PaykitError::Transport {
            context: "failed to send Private Application Message".into(),
            source: anyhow::Error::new(NonRetryablePrivateSendError(
                pubky_noise::PubkyNoiseError::EncryptionError,
            )),
        };

        assert!(err.is_non_retryable_private_send_error());
    }

    #[test]
    fn test_private_receive_retry_classification() {
        assert!(is_retryable_private_receive_error(
            &pubky_noise::PubkyNoiseError::HomeserverResponseError,
        ));

        for err in [
            pubky_noise::PubkyNoiseError::BadLengthCiphertext,
            pubky_noise::PubkyNoiseError::IsHandshake,
            pubky_noise::PubkyNoiseError::DecryptionError,
            pubky_noise::PubkyNoiseError::CounterOverflow,
            pubky_noise::PubkyNoiseError::NonceOverflow,
        ] {
            assert!(
                !is_retryable_private_receive_error(&err),
                "{err:?} should not be retried"
            );
        }
    }

    #[test]
    fn test_non_retryable_private_receive_error_is_classified() {
        let err = PaykitError::Transport {
            context: "failed to receive Private Application Messages".into(),
            source: anyhow::Error::new(NonRetryablePrivateReceiveError(
                pubky_noise::PubkyNoiseError::DecryptionError,
            )),
        };

        assert!(err.is_non_retryable_private_receive_error());
    }

    #[test]
    fn test_private_application_message_size_validation_rejects_oversized_payload() {
        let payload = vec![b'x'; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN + 1];
        let err =
            validate_private_application_message_size(&payload, "Payment Request").unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("exceeds")),
            "expected oversize validation error, got: {err}"
        );
    }

    #[test]
    fn test_private_application_message_size_validation_accepts_payload_at_limit() {
        // Guards against a `>` -> `>=` regression in the send-time size check:
        // a payload of exactly PUBKY_NOISE_MSG_LEN bytes must be accepted.
        let payload = vec![b'x'; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN];
        validate_private_application_message_size(&payload, "Payment Request")
            .expect("payload of exactly PUBKY_NOISE_MSG_LEN bytes should be accepted");
    }

    #[test]
    fn test_private_application_message_size_validation_rejects_oversized_private_payment_list() {
        // A Private Payment List can cross the pubky-noise message ceiling via
        // many small Payment Endpoints rather than one oversized payload.
        // Send-time validation is the documented enforcement point; there is
        // no construction-time size limit. The context string matches the real
        // send path (EncryptedLink::send_private_payment_list_message).
        let mut payment_endpoints = HashMap::new();
        for i in 0..20 {
            payment_endpoints.insert(
                PaymentEndpointIdentifier::new(format!("endpoint-{i:02}")).unwrap(),
                PaymentEndpointPayload::new(format!("payload-{i:02}-{}", "x".repeat(40))),
            );
        }
        let list = PrivatePaymentList::new(payment_endpoints);
        let json = serialize_private_payment_list_json(&list).unwrap();
        assert!(
            json.len() > pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
            "fixture must exceed the pubky-noise message ceiling, got {} bytes",
            json.len()
        );

        let err =
            validate_private_application_message_size(json.as_bytes(), "Private Payment List")
                .unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("exceeds")),
            "expected oversize validation error, got: {err}"
        );
    }

    #[test]
    fn test_private_application_message_keeps_malformed_header() {
        let raw = r#"{"version":"bad","kind":"paykit.payment_request","event_id":"not-a-uuid"}"#;
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string());

        assert_eq!(message.version, None);
        assert_eq!(
            message.known_kind(),
            Some(PrivateMessageKind::PaymentRequest)
        );
        assert_eq!(message.raw_json, raw);
    }

    #[test]
    fn test_private_application_message_keeps_json_without_kind() {
        let raw = r#"{"version":1,"payload":"unknown"}"#;
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string());

        assert_eq!(message.version, Some(1));
        assert_eq!(message.kind, None);
        assert_eq!(message.known_kind(), None);
        assert_eq!(message.raw_json, raw);
    }

    #[test]
    fn test_private_application_message_known_kind_uses_raw_json() {
        let raw = r#"{"version":1,"kind":"paykit.receipt_access"}"#;
        let message = PrivateApplicationMessage {
            version: Some(1),
            kind: Some("paykit.payment_request".to_string()),
            raw_json: raw.to_string(),
        };

        assert_eq!(
            message.known_kind(),
            Some(PrivateMessageKind::ReceiptAccess)
        );
    }

    #[test]
    fn test_private_application_message_debug_redacts_raw_json() {
        let raw = r#"{"version":1,"kind":"paykit.receipt_access","key":"secret"}"#;
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string());
        let debug = format!("{message:?}");

        assert!(!debug.contains("secret"));
        assert!(debug.contains("<redacted:"));
    }

    #[test]
    fn test_private_application_message_keeps_invalid_json() {
        let raw = "not json";
        let message = PrivateApplicationMessage::from_plaintext(raw.to_string());

        assert_eq!(message.version, None);
        assert_eq!(message.kind, None);
        assert_eq!(message.known_kind(), None);
        assert_eq!(message.raw_json, raw);
    }

    #[test]
    fn test_decode_private_application_message_retains_invalid_utf8_marker() {
        let mut raw = [0u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN];
        raw[0] = 0xff;

        let message = decode_private_application_message(&raw).unwrap();

        assert_eq!(message.version, None);
        assert_eq!(message.kind, None);
        assert_eq!(message.known_kind(), None);
        assert_eq!(
            message.invalid_utf8_error(),
            Some("Private Application Message plaintext is not valid UTF-8")
        );
        assert!(message
            .raw_json
            .starts_with(INVALID_UTF8_PRIVATE_MESSAGE_PREFIX));
    }

    // CLAUDE.md contract: public reads treat 404/GONE as absence, never as errors.
    fn server_error(status: StatusCode) -> PubkyError {
        PubkyError::Request(RequestError::Server {
            status,
            message: "test response".into(),
        })
    }

    #[test]
    fn test_is_not_found_matches_not_found_and_gone() {
        assert!(is_not_found(&server_error(StatusCode::NOT_FOUND)));
        assert!(is_not_found(&server_error(StatusCode::GONE)));
    }

    #[test]
    fn test_is_not_found_rejects_other_statuses_and_variants() {
        assert!(!is_not_found(&server_error(
            StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!is_not_found(&server_error(StatusCode::FORBIDDEN)));

        let validation_error = PubkyError::Request(RequestError::Validation {
            message: "invalid request".into(),
        });
        assert!(!is_not_found(&validation_error));
    }
}
