//! Paykit's identifiers (Paykit wire 1.3, 1.4): Payment Endpoint Identifiers and payloads,
//! attempt IDs, Allowance IDs and payment references.
//!
//! Each is validated when it is built and keeps its inner value private, so every value that
//! names a record, a claim or a destination went through its rule.

use pubky_switch_core::enc::{b64, unb64_array};
use pubky_switch_core::json::check_uuid_v4;

use crate::error::{invalid, Error, Result};

/// The longest Payment Endpoint Identifier, in characters (Paykit wire 1.4).
pub const MAX_ENDPOINT_IDENTIFIER_LEN: usize = 64;

/// A Payment Endpoint Identifier (Paykit wire 1.4): ASCII letters and digits, `-`, `_` and
/// `.`, 1 to 64 characters, never `.`, `..` or `private`.
///
/// The recommended form is three lower-case segments, `<asset>-<rail>-<format>`, such as
/// `btc-lightning-bolt11`; requests with conversion terms must use it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaymentEndpointIdentifier(String);

impl PaymentEndpointIdentifier {
    /// Read an identifier from a document (Paykit wire 1.4).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an identifier outside the rule.
    pub fn parse(text: &str) -> Result<Self> {
        let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.');
        if text.is_empty()
            || text.len() > MAX_ENDPOINT_IDENTIFIER_LEN
            || !text.bytes().all(allowed)
            || matches!(text, "." | ".." | "private")
        {
            return invalid(
                "Paykit wire 1.4: a Payment Endpoint Identifier is 1 to 64 of A-Z, a-z, 0-9, -, _ and ., \
                 and not ., .. or private",
            );
        }
        Ok(Self(text.to_owned()))
    }

    /// An identifier the caller supplies (Paykit wire 1.4).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an identifier outside the rule.
    pub fn new(text: &str) -> Result<Self> {
        Self::parse(text).map_err(Error::into_validation)
    }

    /// The identifier's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the identifier has the recommended form, three non-empty lower-case segments
    /// `<asset>-<rail>-<format>` of `a-z` and `0-9` (Paykit wire 1.4).
    pub fn is_recommended_form(&self) -> bool {
        let segments: Vec<&str> = self.0.split('-').collect();
        segments.len() == 3
            && segments.iter().all(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            })
    }

    /// The asset segment of an identifier of the recommended form.
    pub fn asset_segment(&self) -> Option<&str> {
        self.is_recommended_form()
            .then(|| self.0.split('-').next())
            .flatten()
    }
}

impl std::fmt::Display for PaymentEndpointIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Payment Endpoint Payload (Paykit wire 1.4): a non-empty UTF-8 string that Paykit never
/// interprets.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PaymentEndpointPayload(String);

impl std::fmt::Debug for PaymentEndpointPayload {
    // A destination is payment data; logs show only its size.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PaymentEndpointPayload({} bytes)", self.0.len())
    }
}

impl PaymentEndpointPayload {
    /// Read a payload from a document (Paykit wire 1.4).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty payload.
    pub fn parse(text: &str) -> Result<Self> {
        if text.is_empty() {
            return invalid("Paykit wire 1.4: a Payment Endpoint Payload is non-empty");
        }
        Ok(Self(text.to_owned()))
    }

    /// A payload the caller supplies (Paykit wire 1.4).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an empty payload.
    pub fn new(text: &str) -> Result<Self> {
        Self::parse(text).map_err(Error::into_validation)
    }

    /// The payload's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The payload's text, owned.
    pub fn into_inner(self) -> String {
        self.0
    }
}

/// An attempt ID (Paykit wire 1.3): 16 random bytes, `b64(16)` on the wire. It is the
/// claim's attempt ID (Switch wire 6.2), and the payment method's idempotency key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttemptId([u8; 16]);

impl AttemptId {
    /// An attempt ID from its 16 bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// A fresh attempt ID from the secure random generator (Paykit spec 7.2).
    ///
    /// # Errors
    ///
    /// [`Error::Switch`] if the random generator fails.
    pub fn generate() -> Result<Self> {
        Ok(Self(pubky_switch_core::enc::random_bytes::<16>()?))
    }

    /// Read an attempt ID from its text, `b64(16)` (Paykit wire 1.3).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for text that isn't base64url of 16 bytes.
    pub fn parse(text: &str) -> Result<Self> {
        unb64_array::<16>(text)
            .map(Self)
            .map_err(|_| Error::Invalid("Paykit wire 1.3: an attempt ID is b64(16)".into()))
    }

    /// The 16 bytes.
    pub fn to_bytes(self) -> [u8; 16] {
        self.0
    }

    /// The text, `b64(16)`.
    pub fn to_text(self) -> String {
        b64(&self.0)
    }
}

/// An Allowance ID (Paykit wire 1.3): a lower-case UUID v4, the form of the Switch's
/// `msg_id`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AllowanceId(String);

impl AllowanceId {
    /// Read an Allowance ID from a document (Paykit wire 1.3).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for anything but a lower-case UUID v4.
    pub fn parse(text: &str) -> Result<Self> {
        check_uuid_v4(text).map_err(|_| {
            Error::Invalid("Paykit wire 1.3: an allowance_id is a lower-case UUID v4".into())
        })?;
        Ok(Self(text.to_owned()))
    }

    /// An Allowance ID the caller supplies (Paykit wire 1.3).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for anything but a lower-case UUID v4.
    pub fn new(text: &str) -> Result<Self> {
        Self::parse(text).map_err(Error::into_validation)
    }

    /// A fresh Allowance ID (Paykit wire 1.3).
    ///
    /// # Errors
    ///
    /// [`Error::Switch`] if the random generator fails.
    pub fn generate() -> Result<Self> {
        Ok(Self(pubky_switch_core::message::new_msg_id()?))
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AllowanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The longest payment reference, in characters (Paykit wire 4.5).
pub const MAX_PAYMENT_REFERENCE_CHARS: usize = 256;

/// A request's payment reference (Paykit wire 4.5): 1 to 256 characters, without control
/// characters, copied into the payment where the method allows it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentReference(String);

impl PaymentReference {
    /// Read a payment reference from a document (Paykit wire 4.5).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty or longer reference, or one with a control character.
    pub fn parse(text: &str) -> Result<Self> {
        let chars = text.chars().count();
        if chars == 0 || chars > MAX_PAYMENT_REFERENCE_CHARS || text.chars().any(char::is_control) {
            return invalid(
                "Paykit wire 4.5: payment_reference is 1 to 256 characters without control characters",
            );
        }
        Ok(Self(text.to_owned()))
    }

    /// A payment reference the caller supplies (Paykit wire 4.5).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a reference outside the rule.
    pub fn new(text: &str) -> Result<Self> {
        Self::parse(text).map_err(Error::into_validation)
    }

    /// The reference's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endpoint_identifier_takes_the_rule() {
        for ok in ["btc-lightning-bolt11", "a", "A.b_c-9", &"x".repeat(64)] {
            assert!(PaymentEndpointIdentifier::parse(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn test_endpoint_identifier_refuses_outside_the_rule() {
        for bad in [
            "",
            ".",
            "..",
            "private",
            "a/b",
            "a b",
            "\u{e9}",
            &"x".repeat(65),
        ] {
            assert!(PaymentEndpointIdentifier::parse(bad).is_err(), "{bad:?}");
        }
        assert!(matches!(
            PaymentEndpointIdentifier::new("private"),
            Err(Error::Validation(_))
        ));
    }

    #[test]
    fn test_endpoint_identifier_recommended_form() {
        let id = PaymentEndpointIdentifier::parse("btc-lightning-bolt11").unwrap();
        assert!(id.is_recommended_form());
        assert_eq!(id.asset_segment(), Some("btc"));
        for other in [
            "BTC-lightning-bolt11",
            "btc-lightning",
            "btc--x",
            "btc.a-b-c",
        ] {
            let id = PaymentEndpointIdentifier::parse(other).unwrap();
            assert!(!id.is_recommended_form(), "{other}");
            assert_eq!(id.asset_segment(), None);
        }
    }

    #[test]
    fn test_endpoint_payload_is_non_empty() {
        assert!(PaymentEndpointPayload::parse("lnbc1").is_ok());
        assert!(PaymentEndpointPayload::parse("").is_err());
        let payload = PaymentEndpointPayload::parse("secret-address").unwrap();
        assert!(!format!("{payload:?}").contains("secret"));
    }

    #[test]
    fn test_attempt_id_round_trips() {
        let id = AttemptId::parse("mAFdg5_0tVNjmrgiTUZW4g").unwrap();
        assert_eq!(id.to_text(), "mAFdg5_0tVNjmrgiTUZW4g");
        assert!(AttemptId::parse("mAFdg5_0tVNjmrgiTUZW").is_err());
        assert_ne!(
            AttemptId::generate().unwrap(),
            AttemptId::generate().unwrap()
        );
    }

    #[test]
    fn test_allowance_id_is_a_lower_case_uuid_v4() {
        assert!(AllowanceId::parse("d0efffb7-191b-4709-b6c7-e343af5500b4").is_ok());
        assert!(AllowanceId::parse("D0EFFFB7-191B-4709-B6C7-E343AF5500B4").is_err());
        assert!(AllowanceId::parse("d0efffb7-191b-1709-b6c7-e343af5500b4").is_err());
        assert!(AllowanceId::parse(AllowanceId::generate().unwrap().as_str()).is_ok());
    }

    #[test]
    fn test_payment_reference_bounds() {
        assert!(PaymentReference::parse("order-1042").is_ok());
        assert!(PaymentReference::parse(&"\u{e9}".repeat(256)).is_ok());
        assert!(PaymentReference::parse(&"a".repeat(257)).is_err());
        assert!(PaymentReference::parse("").is_err());
        assert!(PaymentReference::parse("a\nb").is_err());
    }
}
