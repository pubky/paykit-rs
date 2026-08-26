use crate::{
    validation::{validate_asset_text, validate_decimal_text},
    Result,
};

/// Amount of value in a payment flow, expressed as decimal text plus asset.
///
/// # Validation
///
/// [`PaymentAmount::new`] accepts a `value` consisting only of ASCII digits and
/// at most one decimal point, with at least one digit. Leading `+` and `-` signs
/// are rejected. Values such as `.5` and `10.` are accepted. The `asset` must be
/// non-empty and contain no control characters. Beyond these checks, Paykit
/// defines no range, precision, scale, normalization, or asset-registry policy.
///
/// [`PaymentAmount::new`] validates only during construction. Direct struct
/// construction and later field mutation are unchecked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentAmount {
    /// Decimal string, such as `10.00`. Mutation is not revalidated.
    pub value: String,
    /// Asset code or unit, such as `usd`, `btc`, or `usdt`. Mutation is not revalidated.
    pub asset: String,
}

impl PaymentAmount {
    /// Create a Payment Amount after validating its value and asset fields.
    pub fn new(value: impl Into<String>, asset: impl Into<String>) -> Result<Self> {
        let amount = Self {
            value: value.into(),
            asset: asset.into(),
        };
        amount.validate_with_label("Payment Amount")?;
        Ok(amount)
    }

    pub(crate) fn validate_with_label(&self, label: &'static str) -> Result<()> {
        validate_decimal_text(&self.value, &format!("{label}.value"))?;
        validate_asset_text(&self.asset, "Payment Amount asset")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PaykitError;

    #[test]
    fn payment_amount_accepts_decimal_value_and_asset() {
        let amount = PaymentAmount::new("10.00", "usd").unwrap();
        assert_eq!(amount.value, "10.00");
        assert_eq!(amount.asset, "usd");
    }

    #[test]
    fn payment_amount_rejects_invalid_value() {
        let err = PaymentAmount::new("ten", "usd").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("decimal string")));
    }

    #[test]
    fn payment_amount_rejects_empty_asset() {
        let err = PaymentAmount::new("10.00", "").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("must not be empty")));
    }

    #[test]
    fn payment_amount_rejects_control_character_asset() {
        let err = PaymentAmount::new("10.00", "usd\n").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("control characters")));
    }

    #[test]
    fn payment_amount_rejects_multiple_decimal_points() {
        let err = PaymentAmount::new("1.2.3", "usd").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("decimal string")));
    }

    #[test]
    fn payment_amount_rejects_lone_decimal_point() {
        let err = PaymentAmount::new(".", "usd").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("at least one digit")));
    }

    // Pin of currently-accepted behavior: values with a bare leading or
    // trailing decimal point (".5", "10.") pass validation today. Tightening
    // this is a protocol decision; this test makes any such change a visible
    // diff rather than a silent one.
    #[test]
    fn test_payment_amount_bare_decimal_point_currently_accepted() {
        let leading = PaymentAmount::new(".5", "usd").unwrap();
        assert_eq!(leading.value, ".5");
        let trailing = PaymentAmount::new("10.", "usd").unwrap();
        assert_eq!(trailing.value, "10.");
    }
}
