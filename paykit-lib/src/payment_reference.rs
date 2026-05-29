use crate::{PaykitError, Result};

/// UUID-v4 correlation reference used to connect Private Payment Envelopes and receipts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentReference(String);

impl PaymentReference {
    /// Create a Payment Reference after validating that the input is a UUID v4 string.
    ///
    /// Accepted UUID-v4 inputs are canonicalized to lowercase hyphenated form.
    pub fn new(reference: impl Into<String>) -> Result<Self> {
        let reference = reference.into();
        let uuid = uuid::Uuid::try_parse(&reference).map_err(|err| {
            PaykitError::Validation(format!("Payment Reference must be a UUID v4 string: {err}"))
        })?;
        if uuid.get_version_num() != 4 || uuid.get_variant() != uuid::Variant::RFC4122 {
            return Err(PaykitError::Validation(
                "Payment Reference must be an RFC4122 UUID v4 string".into(),
            ));
        }
        Ok(Self(uuid.hyphenated().to_string()))
    }

    /// Generate a fresh random UUID-v4 Payment Reference.
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Access the inner UUID string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaymentReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PaymentReference {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PaykitError;

    #[test]
    fn test_payment_reference_accepts_uuid_v4() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(reference.as_str(), "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(
            format!("{reference}"),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_payment_reference_canonicalizes_uuid_v4() {
        let reference = PaymentReference::new("550E8400-E29B-41D4-A716-446655440000").unwrap();
        assert_eq!(reference.as_str(), "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn test_payment_reference_rejects_non_uuid() {
        let err = PaymentReference::new("not-a-uuid").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("UUID v4")));
    }

    #[test]
    fn test_payment_reference_rejects_uuid_v1() {
        let err = PaymentReference::new("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("UUID v4")));
    }

    #[test]
    fn test_payment_reference_rejects_non_rfc4122_variant() {
        let err = PaymentReference::new("550e8400-e29b-41d4-0716-446655440000").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("RFC4122 UUID v4")));
    }
}
