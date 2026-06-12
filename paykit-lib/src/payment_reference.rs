use crate::{PaykitError, Result};

/// Payee-visible correlation reference used to connect payments with Paykit artifacts.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PaymentReference(String);

/// Maximum Payment Reference length in Unicode scalar values.
pub const PAYMENT_REFERENCE_MAX_LEN: usize = 256;

impl PaymentReference {
    /// Create a Payment Reference after validating that the input is non-empty,
    /// bounded, and free of control characters.
    pub fn new(reference: impl Into<String>) -> Result<Self> {
        let reference = reference.into();
        if reference.is_empty() {
            return Err(PaykitError::Validation(
                "Payment Reference must not be empty".into(),
            ));
        }
        let char_count = reference.chars().count();
        if char_count > PAYMENT_REFERENCE_MAX_LEN {
            return Err(PaykitError::Validation(format!(
                "Payment Reference must not exceed {PAYMENT_REFERENCE_MAX_LEN} characters, got {char_count}"
            )));
        }
        if let Some((pos, ch)) = reference.char_indices().find(|&(_, ch)| ch.is_control()) {
            return Err(PaykitError::Validation(format!(
                "Payment Reference must not contain control character U+{:04X} at byte {pos}",
                ch as u32
            )));
        }
        Ok(Self(reference))
    }

    /// Access the inner reference string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaymentReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Debug for PaymentReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PaymentReference(<redacted>)")
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
    fn test_payment_reference_accepts_text() {
        let reference = PaymentReference::new("invoice 2026/0001").unwrap();
        assert_eq!(reference.as_str(), "invoice 2026/0001");
        assert_eq!(format!("{reference}"), "invoice 2026/0001");
        assert_eq!(format!("{reference:?}"), "PaymentReference(<redacted>)");
    }

    #[test]
    fn test_payment_reference_rejects_empty() {
        let err = PaymentReference::new("").unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("must not be empty"))
        );
    }

    #[test]
    fn test_payment_reference_rejects_control_characters() {
        let err = PaymentReference::new("invoice\n2026").unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("control character"))
        );
    }

    #[test]
    fn test_payment_reference_rejects_over_max_length() {
        let reference = "a".repeat(PAYMENT_REFERENCE_MAX_LEN + 1);
        let err = PaymentReference::new(reference).unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("must not exceed")));
    }
}
