use crate::{PaykitError, Result};

/// Amount of value in a payment flow, expressed as decimal text plus asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentAmount {
    /// Decimal string, such as `10.00`.
    pub value: String,
    /// Asset code or unit, such as `usd`, `btc`, or `usdt`.
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
        validate_decimal(&self.value, label)?;
        validate_text(&self.asset, "asset")
    }
}

fn validate_decimal(value: &str, label: &'static str) -> Result<()> {
    let mut seen_dot = false;
    let mut seen_digit = false;
    for ch in value.chars() {
        if ch == '.' && !seen_dot {
            seen_dot = true;
        } else if ch.is_ascii_digit() {
            seen_digit = true;
        } else {
            return Err(PaykitError::Validation(format!(
                "{label}.value must be a decimal string"
            )));
        }
    }
    if !seen_digit {
        return Err(PaykitError::Validation(format!(
            "{label}.value must contain at least one digit"
        )));
    }
    Ok(())
}

fn validate_text(value: &str, field: &'static str) -> Result<()> {
    if value.is_empty() {
        return Err(PaykitError::Validation(format!(
            "Payment Amount {field} must not be empty"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(PaykitError::Validation(format!(
            "Payment Amount {field} must not contain control characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
