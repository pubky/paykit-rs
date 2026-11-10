//! Amounts, assets and rates (Paykit wire 1.2, 4.4).
//!
//! Amounts are decimal strings. They are compared and added exactly, as integers scaled by
//! 10^18 in a `u128`; an addition that overflows fails closed. The original spelling is kept,
//! since it is what a sender signed, and two spellings of one value compare equal.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use crate::error::{invalid, Error, Result};

/// The scale of an amount: amounts are integers in units of 10^-18 (Paykit wire 1.2).
pub const SCALE: u128 = 1_000_000_000_000_000_000;

/// The most digits after the `.` of an amount (Paykit wire 1.2).
pub const MAX_FRACTION_DIGITS: usize = 18;

/// A decimal amount (Paykit wire 1.2): `[0-9]+(\.[0-9]*)?` or `\.[0-9]+`, at most 18
/// fraction digits, and the value times 10^18 below 2^128.
///
/// It keeps its original spelling, which a sender signed, and compares by value: `25.00`
/// equals `25`.
#[derive(Clone, Debug)]
pub struct Decimal {
    text: String,
    scaled: u128,
}

impl Decimal {
    /// Read an amount from a message, a record or another document (Paykit wire 1.2).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a text outside the grammar or the bounds.
    pub fn parse(text: &str) -> Result<Self> {
        let scaled = parse_scaled(text)?;
        Ok(Self {
            text: text.to_owned(),
            scaled,
        })
    }

    /// An amount the caller supplies (Paykit wire 1.2).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a text outside the grammar or the bounds.
    pub fn new(text: &str) -> Result<Self> {
        Self::parse(text).map_err(Error::into_validation)
    }

    /// The amount of a scaled value, in the normalized spelling (Paykit wire 1.2).
    pub fn from_scaled(scaled: u128) -> Self {
        Self {
            text: normalize(scaled),
            scaled,
        }
    }

    /// Zero, spelled `0`.
    pub fn zero() -> Self {
        Self::from_scaled(0)
    }

    /// The spelling as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The value times 10^18.
    pub fn scaled(&self) -> u128 {
        self.scaled
    }

    /// The normalized spelling (Paykit wire 1.2): no leading zeros before a non-zero integer
    /// digit, no trailing fraction zeros, no `.` without a fraction, and `0` for zero.
    pub fn normalized(&self) -> String {
        normalize(self.scaled)
    }

    /// Whether the amount is zero.
    pub fn is_zero(&self) -> bool {
        self.scaled == 0
    }

    /// The exact sum, in the normalized spelling; `None` on overflow, which fails closed
    /// (Paykit wire 1.2).
    pub fn checked_add(&self, other: &Decimal) -> Option<Decimal> {
        self.scaled.checked_add(other.scaled).map(Self::from_scaled)
    }
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        self.scaled == other.scaled
    }
}

impl Eq for Decimal {}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        self.scaled.cmp(&other.scaled)
    }
}

impl Hash for Decimal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.scaled.hash(state);
    }
}

impl std::fmt::Display for Decimal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Parse an amount into its value times 10^18 (Paykit wire 1.2).
fn parse_scaled(text: &str) -> Result<u128> {
    let (int, frac) = match text.split_once('.') {
        Some((int, frac)) => (int, frac),
        None => (text, ""),
    };
    if !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return invalid("Paykit wire 1.2: an amount holds only ASCII digits and one point");
    }
    if int.is_empty() && frac.is_empty() {
        return invalid("Paykit wire 1.2: an amount holds at least one digit");
    }
    if frac.len() > MAX_FRACTION_DIGITS {
        return invalid("Paykit wire 1.2: an amount has at most 18 fraction digits");
    }
    let too_large =
        || Error::Invalid("Paykit wire 1.2: an amount times 10^18 is below 2^128".into());
    let mut whole: u128 = 0;
    for digit in int.bytes() {
        whole = whole
            .checked_mul(10)
            .and_then(|v| v.checked_add(u128::from(digit - b'0')))
            .ok_or_else(too_large)?;
    }
    let mut fraction: u128 = 0;
    for digit in frac.bytes() {
        // At most 18 digits: below 10^18, no overflow.
        fraction = fraction * 10 + u128::from(digit - b'0');
    }
    // `frac.len()` is at most 18, so the exponent is at most 18.
    let pad = u32::try_from(MAX_FRACTION_DIGITS - frac.len()).unwrap_or(0);
    fraction *= 10u128.pow(pad);
    whole
        .checked_mul(SCALE)
        .and_then(|v| v.checked_add(fraction))
        .ok_or_else(too_large)
}

/// The normalized spelling of a scaled amount (Paykit wire 1.2).
pub fn normalize(scaled: u128) -> String {
    let int = scaled / SCALE;
    let frac = scaled % SCALE;
    if frac == 0 {
        return int.to_string();
    }
    let digits = format!("{frac:018}");
    format!("{int}.{}", digits.trim_end_matches('0'))
}

/// An asset (Paykit wire 1.2): non-empty, without control characters, compared exactly
/// and case-sensitively. By convention the lower-case asset segment of the Payment Endpoint
/// Identifiers, such as `btc` or `usd`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Asset(String);

impl Asset {
    /// Read an asset from a document (Paykit wire 1.2).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an empty asset or one with a control character.
    pub fn parse(text: &str) -> Result<Self> {
        if text.is_empty() || text.chars().any(char::is_control) {
            return invalid("Paykit wire 1.2: an asset is non-empty, without control characters");
        }
        Ok(Self(text.to_owned()))
    }

    /// An asset the caller supplies (Paykit wire 1.2).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an empty asset or one with a control character.
    pub fn new(text: &str) -> Result<Self> {
        Self::parse(text).map_err(Error::into_validation)
    }

    /// The asset's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Asset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An amount in an asset (Paykit wire 4.4): `{ "value": amount, "asset": string }`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentAmount {
    /// The amount, in whole units of the asset.
    pub value: Decimal,
    /// The asset.
    pub asset: Asset,
}

/// A conversion rate (Paykit wire 1.2, 4.4): an asset selector and a value above zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rate {
    asset: Asset,
    value: Decimal,
}

impl Rate {
    /// A rate the caller supplies, whose value must be above zero (Paykit wire 1.2).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a zero value.
    pub fn new(asset: Asset, value: Decimal) -> Result<Self> {
        Self::read(asset, value).map_err(Error::into_validation)
    }

    /// A rate read from a document (Paykit wire 1.2).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a zero value.
    pub(crate) fn read(asset: Asset, value: Decimal) -> Result<Self> {
        if value.is_zero() {
            return invalid("Paykit wire 1.2: a rate is above zero");
        }
        Ok(Self { asset, value })
    }

    /// The asset selector.
    pub fn asset(&self) -> &Asset {
        &self.asset
    }

    /// The rate's value.
    pub fn value(&self) -> &Decimal {
        &self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_amount_parses_the_grammar() {
        for (text, normalized) in [
            ("0", "0"),
            ("25.00", "25"),
            (".5", "0.5"),
            ("10.", "10"),
            ("000123.4500", "123.45"),
            ("0.000000000000000001", "0.000000000000000001"),
        ] {
            let amount = Decimal::parse(text).unwrap();
            assert_eq!(amount.normalized(), normalized, "{text}");
            assert_eq!(amount.as_str(), text);
        }
    }

    #[test]
    fn test_amount_refuses_outside_the_grammar() {
        for text in [
            "",
            ".",
            "-1",
            "+1",
            "1e3",
            "1,000",
            " 1",
            "1.2.3",
            "0.0000000000000000001",
        ] {
            assert!(Decimal::parse(text).is_err(), "{text}");
        }
        assert!(Decimal::parse("340282366920938463463.374607431768211456").is_err());
        assert!(Decimal::parse("340282366920938463463.374607431768211455").is_ok());
        assert!(Decimal::parse("999999999999999999999999999999999999999999").is_err());
    }

    #[test]
    fn test_amount_compares_by_value() {
        assert_eq!(
            Decimal::parse("25.00").unwrap(),
            Decimal::parse("25").unwrap()
        );
        assert!(Decimal::parse("0.1").unwrap() < Decimal::parse("0.11").unwrap());
    }

    #[test]
    fn test_amount_addition_fails_closed() {
        let max = Decimal::from_scaled(u128::MAX);
        assert!(max.checked_add(&Decimal::from_scaled(1)).is_none());
        let sum = Decimal::parse("0.01")
            .unwrap()
            .checked_add(&Decimal::parse("0.02").unwrap())
            .unwrap();
        assert_eq!(sum.as_str(), "0.03");
    }

    #[test]
    fn test_amount_new_is_a_validation_error() {
        assert!(matches!(Decimal::new("x"), Err(Error::Validation(_))));
        assert!(matches!(Decimal::parse("x"), Err(Error::Invalid(_))));
    }

    #[test]
    fn test_asset_refuses_empty_and_control_characters() {
        assert!(Asset::parse("btc").is_ok());
        assert!(Asset::parse("").is_err());
        assert!(Asset::parse("bt\u{7}c").is_err());
        assert!(Asset::parse("btc\u{85}").is_err());
    }

    #[test]
    fn test_rate_is_above_zero() {
        let btc = Asset::parse("btc").unwrap();
        assert!(Rate::new(btc.clone(), Decimal::parse("0.1").unwrap()).is_ok());
        assert!(Rate::new(btc.clone(), Decimal::parse("0.000").unwrap()).is_err());
    }

    #[test]
    fn test_rate_error_class_follows_its_source() {
        let btc = Asset::parse("btc").unwrap();
        let zero = Decimal::parse("0").unwrap();
        assert!(matches!(
            Rate::new(btc.clone(), zero.clone()),
            Err(Error::Validation(_))
        ));
        assert!(matches!(Rate::read(btc, zero), Err(Error::Invalid(_))));
    }
}
