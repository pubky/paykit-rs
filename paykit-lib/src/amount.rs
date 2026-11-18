//! Amounts, assets and rates (Paykit wire 1.2, 4.4).
//!
//! Amounts are decimal strings. They are compared and added exactly, as integers scaled by
//! 10^18 in a `u128`; an addition that overflows fails closed. The original spelling is kept,
//! since it is what a sender signed, and two spellings of one value compare equal. The amount
//! to pay to an endpoint is priced the same way ([`amount_for_endpoint`]).

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use crate::error::{invalid, Error, Result};
use crate::identifiers::PaymentEndpointIdentifier;

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
    /// A rate the caller supplies: its selector is `<asset>` or `<asset>-<rail>`, each
    /// segment lower-case letters and digits, and its value is above zero (Paykit wire 4.4).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a malformed selector or a zero value.
    pub fn new(asset: Asset, value: Decimal) -> Result<Self> {
        Self::read(asset, value).map_err(Error::into_validation)
    }

    /// A rate read from a document (Paykit wire 1.2, 4.4).
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a malformed selector or a zero value.
    pub(crate) fn read(asset: Asset, value: Decimal) -> Result<Self> {
        let selector: Vec<&str> = asset.as_str().split('-').collect();
        if !(1..=2).contains(&selector.len()) || !selector.iter().all(|s| is_conversion_segment(s))
        {
            return invalid(
                "Paykit wire 4.4: a rate's asset is <asset> or <asset>-<rail>, each segment lower-case letters and digits",
            );
        }
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

/// Whether `text` is one segment of the conversion convention: non-empty, lower-case ASCII
/// letters and digits (Paykit wire 4.4, `specs/payment-conversion.md`).
pub(crate) fn is_conversion_segment(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// What a payment of a request pays to an endpoint ([`amount_for_endpoint`], Paykit spec 7.2
/// step 1, Paykit wire 4.4, 4.9).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EndpointAmount {
    /// The endpoint is priced by `rate`, the asset-rail rate if there is one, else the
    /// asset-wide one: `amount` is the requested value times the rate, rounded up once to the
    /// endpoint's smallest unit, in the endpoint's asset. Its user confirms it before paying.
    ByRate {
        /// The rate used.
        rate: Rate,
        /// The amount to pay, in the endpoint's asset.
        amount: PaymentAmount,
    },
    /// The endpoint is of the requested asset and no rate prices it: the requested amount,
    /// rounded up once to the endpoint's smallest unit.
    RequestedAsset(PaymentAmount),
    /// The request has no conversion terms and the endpoint is of another asset, or of one
    /// that can't be told: the request promises no rate, so the payer pays it only by hand,
    /// by its own calculation, which its user confirms. An app may decline it.
    ByHandOnly,
    /// Under conversion terms, the endpoint is of another asset that no rate prices, or of
    /// one that can't be told: it is unusable for the payment. A market rate is never used.
    Unusable,
}

/// The rate of `rates` that prices `endpoint` (Paykit wire 4.4): the asset-rail rate,
/// `usdt-polygon` for `usdt-polygon-address`, if there is one, else the asset-wide rate,
/// `usdt`, whatever the order of `rates`; `None` for none, or for an endpoint not of the form
/// `<asset>-<rail>-<format>`, whose asset can't be told. The format segment never selects a
/// rate, and a rail-qualified rate prices only its own rail. It is the selection
/// [`amount_for_endpoint`] prices by.
///
/// # Errors
///
/// [`Error::Validation`] for rates that are empty or name a selector twice, which select
/// nothing: neither is taken as no rate or as the first match.
///
/// # Examples
///
/// ```
/// use paykit_lib::{rate_for_endpoint, Asset, Decimal, PaymentEndpointIdentifier, Rate};
///
/// let rate = |asset: &str, value: &str| Rate::new(Asset::new(asset)?, Decimal::new(value)?);
/// let rates = [rate("usdt", "1")?, rate("usdt-polygon", "1.008")?];
/// let polygon = PaymentEndpointIdentifier::new("usdt-polygon-address")?;
/// assert_eq!(rate_for_endpoint(&rates, &polygon)?, Some(&rates[1]));
/// let sepa = PaymentEndpointIdentifier::new("eur-sepa-iban")?;
/// assert_eq!(rate_for_endpoint(&rates, &sepa)?, None);
/// assert!(rate_for_endpoint(&[], &polygon).is_err());
/// # Ok::<(), paykit_lib::Error>(())
/// ```
pub fn rate_for_endpoint<'a>(
    rates: &'a [Rate],
    endpoint: &PaymentEndpointIdentifier,
) -> Result<Option<&'a Rate>> {
    if rates.is_empty() {
        return Err(Error::Validation(
            "Paykit wire 4.4: rates are non-empty when present".into(),
        ));
    }
    for (i, rate) in rates.iter().enumerate() {
        if rates[..i].iter().any(|r| r.asset == rate.asset) {
            return Err(Error::Validation(
                "Paykit wire 4.4: rates name each asset selector once".into(),
            ));
        }
    }
    let Some(asset) = endpoint.asset_segment() else {
        return Ok(None);
    };
    let rail = endpoint.as_str().split('-').nth(1).unwrap_or_default();
    let asset_rail = format!("{asset}-{rail}");
    Ok(rates
        .iter()
        .find(|r| r.asset.as_str() == asset_rail)
        .or_else(|| rates.iter().find(|r| r.asset.as_str() == asset)))
}

/// What a payment of a request for `requested` pays to `endpoint` (Paykit spec 7.2 step 1,
/// Paykit wire 4.4, 4.9, `specs/payment-conversion.md`), as payer and as payee alike.
/// `rates` are the conversion terms' rates, the request's fixed rates or the rates of the
/// period details being paid; `None` for a request without conversion terms.
///
/// `endpoint` is the selected endpoint's Payment Endpoint Identifier, of the form
/// `<asset>-<rail>-<format>`, and `fraction_digits` the number of fraction digits of its
/// smallest unit, from 0 to 18, which the payment method names: 8 for satoshis, 11 for
/// millisatoshis, 6 for a token with six decimals. The outcome is, in this order:
///
/// 1. [`EndpointAmount::ByRate`] by the rate [`rate_for_endpoint`] selects: the asset-rail
///    rate, else the asset-wide one;
/// 2. else [`EndpointAmount::RequestedAsset`], when the endpoint's asset is the requested
///    asset;
/// 3. else, without conversion terms, [`EndpointAmount::ByHandOnly`]: the request promises
///    no rate, so the payment is made only by hand, by the payer's own calculation;
/// 4. else [`EndpointAmount::Unusable`]. A market rate is never used.
///
/// The requested value times the rate, or 1, is computed exactly and rounded up once, to the
/// endpoint's smallest unit. The amount is in the endpoint's asset segment, normalized.
///
/// It checks neither that the request accepts the endpoint nor that the period details are
/// the ones to use; the caller does.
///
/// # Errors
///
/// [`Error::Validation`] for `fraction_digits` above 18, explicit rates that are empty or
/// repeat a selector ([`rate_for_endpoint`]), or an amount to pay whose value times 10^18
/// isn't below 2^128.
///
/// # Examples
///
/// ```
/// use paykit_lib::{
///     amount_for_endpoint, Asset, Decimal, EndpointAmount, PaymentAmount,
///     PaymentEndpointIdentifier, Rate,
/// };
///
/// let requested = PaymentAmount {
///     value: Decimal::new("0.05")?,
///     asset: Asset::new("usd")?,
/// };
/// let rates = [Rate::new(Asset::new("btc")?, Decimal::new("0.000012345")?)?];
/// let onchain = PaymentEndpointIdentifier::new("btc-onchain-address")?;
/// // 61.725 satoshis, rounded up to 62.
/// let EndpointAmount::ByRate { rate, amount } =
///     amount_for_endpoint(&requested, Some(&rates), &onchain, 8)?
/// else {
///     unreachable!()
/// };
/// assert_eq!(rate, rates[0]);
/// assert_eq!(amount.value.as_str(), "0.00000062");
/// assert_eq!(amount.asset.as_str(), "btc");
/// // No rate prices eur, and eur isn't the requested asset.
/// let sepa = PaymentEndpointIdentifier::new("eur-sepa-iban")?;
/// assert_eq!(
///     amount_for_endpoint(&requested, Some(&rates), &sepa, 2)?,
///     EndpointAmount::Unusable
/// );
/// // Without conversion terms, only by hand.
/// assert_eq!(
///     amount_for_endpoint(&requested, None, &sepa, 2)?,
///     EndpointAmount::ByHandOnly
/// );
/// # Ok::<(), paykit_lib::Error>(())
/// ```
pub fn amount_for_endpoint(
    requested: &PaymentAmount,
    rates: Option<&[Rate]>,
    endpoint: &PaymentEndpointIdentifier,
    fraction_digits: u32,
) -> Result<EndpointAmount> {
    let validation = |why: &str| Err(Error::Validation(why.into()));
    let digits = usize::try_from(fraction_digits).unwrap_or(usize::MAX);
    if digits > MAX_FRACTION_DIGITS {
        return validation(
            "Paykit wire 1.2: an endpoint's smallest unit has at most 18 fraction digits",
        );
    }
    let rate = match rates {
        Some(rates) => rate_for_endpoint(rates, endpoint)?,
        None => None,
    };
    let (asset, rate, by_rate) = match (endpoint.asset_segment(), rate) {
        (Some(asset), Some(rate)) => (asset, rate.value.scaled, Some(rate)),
        (Some(asset), None) if requested.asset.as_str() == asset => (asset, SCALE, None),
        _ if rates.is_none() => return Ok(EndpointAmount::ByHandOnly),
        _ => return Ok(EndpointAmount::Unusable),
    };
    // `digits` is at most 18, so the exponent is at most 18.
    let unit = 10u128.pow(u32::try_from(MAX_FRACTION_DIGITS - digits).unwrap_or(0));
    let Some(scaled) = product_rounded_up(requested.value.scaled, rate, unit) else {
        return validation("Paykit wire 1.2: the amount to pay times 10^18 is below 2^128");
    };
    let amount = PaymentAmount {
        value: Decimal::from_scaled(scaled),
        asset: Asset(asset.to_owned()),
    };
    Ok(match by_rate {
        Some(rate) => EndpointAmount::ByRate {
            rate: rate.clone(),
            amount,
        },
        None => EndpointAmount::RequestedAsset(amount),
    })
}

/// The product of two amounts scaled by 10^18, itself scaled by 10^18 and rounded up to a
/// multiple of `unit`; `None` when it isn't below 2^128.
///
/// With `a = a1 * 10^18 + a0` and `b` likewise, `a * b / 10^18` is
/// `a1 * b1 * 10^18 + a1 * b0 + a0 * b1 + a0 * b0 / 10^18`: every term but the last is a
/// whole number, and `a0 * b0` is below 10^36, so only the last is rounded. Each term is at
/// most the result, so a term that overflows means the result does. Rounding up to 10^-18
/// and then to `unit` is rounding up once to `unit`.
fn product_rounded_up(a: u128, b: u128, unit: u128) -> Option<u128> {
    let (a1, a0) = (a / SCALE, a % SCALE);
    let (b1, b0) = (b / SCALE, b % SCALE);
    let low = a0 * b0;
    a1.checked_mul(b1)?
        .checked_mul(SCALE)?
        .checked_add(a1.checked_mul(b0)?)?
        .checked_add(a0.checked_mul(b1)?)?
        .checked_add(low.div_ceil(SCALE))?
        .div_ceil(unit)
        .checked_mul(unit)
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

    fn amount(value: &str, asset: &str) -> PaymentAmount {
        PaymentAmount {
            value: Decimal::parse(value).unwrap(),
            asset: Asset::parse(asset).unwrap(),
        }
    }

    fn rates(list: &[(&str, &str)]) -> Vec<Rate> {
        list.iter()
            .map(|(asset, value)| {
                Rate::new(Asset::parse(asset).unwrap(), Decimal::parse(value).unwrap()).unwrap()
            })
            .collect()
    }

    /// What a payment pays to `endpoint`: `<value> <asset>`, with ` by <selector>` for an
    /// amount a rate prices, `by hand only` or `unusable`.
    fn pay(
        requested: &PaymentAmount,
        rates: Option<&[Rate]>,
        endpoint: &str,
        digits: u32,
    ) -> Result<String> {
        let endpoint = PaymentEndpointIdentifier::parse(endpoint).unwrap();
        Ok(
            match amount_for_endpoint(requested, rates, &endpoint, digits)? {
                EndpointAmount::ByRate { rate, amount } => {
                    format!("{} {} by {}", amount.value, amount.asset, rate.asset)
                }
                EndpointAmount::RequestedAsset(amount) => {
                    format!("{} {}", amount.value, amount.asset)
                }
                EndpointAmount::ByHandOnly => "by hand only".into(),
                EndpointAmount::Unusable => "unusable".into(),
            },
        )
    }

    #[test]
    fn test_amount_for_endpoint_prefers_the_asset_rail_rate() {
        let usd = amount("10", "usd");
        let both = rates(&[("usdt", "1"), ("usdt-polygon", "1.008")]);
        let reversed = rates(&[("usdt-polygon", "1.008"), ("usdt", "1")]);
        for list in [&both, &reversed] {
            let got = pay(&usd, Some(list), "usdt-polygon-address", 6).unwrap();
            assert_eq!(got, "10.08 usdt by usdt-polygon");
            // The format segment selects no rate.
            let got = pay(&usd, Some(list), "usdt-polygon-erc20", 6).unwrap();
            assert_eq!(got, "10.08 usdt by usdt-polygon");
        }
    }

    #[test]
    fn test_amount_for_endpoint_falls_back_to_the_asset_wide_rate() {
        let usd = amount("10", "usd");
        let both = rates(&[("usdt", "1"), ("usdt-polygon", "1.008")]);
        let got = pay(&usd, Some(&both), "usdt-ethereum-address", 6).unwrap();
        assert_eq!(got, "10 usdt by usdt");
    }

    #[test]
    fn test_amount_for_endpoint_pays_the_requested_asset_one_to_one() {
        let usdt = amount("10", "usdt");
        let polygon = rates(&[("usdt-polygon", "1.008")]);
        // An explicit rate for the requested asset takes precedence over parity.
        let got = pay(&usdt, Some(&polygon), "usdt-polygon-address", 6).unwrap();
        assert_eq!(got, "10.08 usdt by usdt-polygon");
        let got = pay(&usdt, Some(&polygon), "usdt-ethereum-address", 6).unwrap();
        assert_eq!(got, "10 usdt");
        let got = pay(&usdt, None, "usdt-ethereum-address", 6).unwrap();
        assert_eq!(got, "10 usdt");
    }

    #[test]
    fn test_amount_for_endpoint_leaves_an_unpriced_asset_unusable() {
        let usd = amount("10", "usd");
        let polygon = rates(&[("usdt-polygon", "1.008")]);
        // Another rail of the asset doesn't inherit a rail-qualified rate.
        let got = pay(&usd, Some(&polygon), "usdt-ethereum-address", 6).unwrap();
        assert_eq!(got, "unusable");
        let got = pay(&usd, Some(&polygon), "eur-sepa-iban", 2).unwrap();
        assert_eq!(got, "unusable");
    }

    #[test]
    fn test_amount_for_endpoint_rounds_up_to_the_smallest_unit() {
        let usd = amount("0.05", "usd");
        let btc = rates(&[("btc", "0.000012345")]);
        // 61.725 satoshis on chain round up to 62; 61,725 millisatoshis are exact.
        let got = pay(&usd, Some(&btc), "btc-onchain-address", 8).unwrap();
        assert_eq!(got, "0.00000062 btc by btc");
        let got = pay(&usd, Some(&btc), "btc-lightning-invoice", 11).unwrap();
        assert_eq!(got, "0.00000061725 btc by btc");
        let usdt = rates(&[("usdt", "1")]);
        let got = pay(&usd, Some(&usdt), "usdt-polygon-address", 6).unwrap();
        assert_eq!(got, "0.05 usdt by usdt");
        // A product below 10^-18 rounds up, and a same-asset payment rounds up too.
        let tiny = amount("0.000000000000000001", "usd");
        let half = rates(&[("btc", "0.5")]);
        let got = pay(&tiny, Some(&half), "btc-onchain-address", 18).unwrap();
        assert_eq!(got, "0.000000000000000001 btc by btc");
        let got = pay(&tiny, Some(&half), "btc-onchain-address", 0).unwrap();
        assert_eq!(got, "1 btc by btc");
        let got = pay(
            &amount("10.0000001", "usdt"),
            None,
            "usdt-polygon-address",
            6,
        )
        .unwrap();
        assert_eq!(got, "10.000001 usdt");
        let got = pay(&amount("0", "usd"), Some(&btc), "btc-onchain-address", 8).unwrap();
        assert_eq!(got, "0 btc by btc");
    }

    #[test]
    fn test_amount_for_endpoint_fails_closed_on_overflow() {
        let max = PaymentAmount {
            value: Decimal::from_scaled(u128::MAX),
            asset: Asset::parse("usd").unwrap(),
        };
        let double = rates(&[("btc", "2")]);
        let error = pay(&max, Some(&double), "btc-onchain-address", 8).unwrap_err();
        assert!(matches!(error, Error::Validation(_)), "{error}");
        // Rounding up past 2^128 overflows too.
        let error = pay(&max, None, "usd-ach-account", 0).unwrap_err();
        assert!(matches!(error, Error::Validation(_)), "{error}");
        assert!(pay(&max, None, "usd-ach-account", 18).is_ok());
    }

    #[test]
    fn test_amount_for_endpoint_refuses_input_outside_its_rules() {
        let usd = amount("10", "usd");
        let usdt = rates(&[("usdt", "1")]);
        let error = pay(&usd, Some(&usdt), "usdt-polygon-address", 19).unwrap_err();
        assert!(matches!(error, Error::Validation(_)), "{error}");
        let twice = rates(&[("usdt", "1"), ("usdt", "1.01")]);
        let error = pay(&usd, Some(&twice), "usdt-polygon-address", 6).unwrap_err();
        assert!(matches!(error, Error::Validation(_)), "{error}");
    }

    /// A selection case: the amount requested, the rates, the endpoint, and the outcome
    /// [`pay`] gives, `None` for a refusal.
    type Case<'a> = (
        &'a PaymentAmount,
        Option<&'a [Rate]>,
        &'a str,
        Option<&'a str>,
    );

    /// Every outcome of the one selection, payer and payee alike (Paykit spec 7.2 step 1):
    /// the rate it uses, the amount, the by-hand-only outcome of a request without
    /// conversion terms, and the refusal of malformed rates, which never select a rate.
    #[test]
    fn test_amount_for_endpoint_selection_outcomes() {
        let usd = amount("10", "usd");
        let usdt = amount("10", "usdt");
        let both = rates(&[("usdt", "1"), ("usdt-polygon", "1.008")]);
        let polygon = rates(&[("usdt-polygon", "1.008")]);
        let twice = rates(&[("usdt-polygon", "1.008"), ("usdt-polygon", "1")]);
        let twice_wide = rates(&[("usdt", "1"), ("usdt", "2")]);
        let empty: Vec<Rate> = Vec::new();
        let cases: [Case<'_>; 16] = [
            // By rate: the asset-rail rate first, then the asset-wide one.
            (
                &usd,
                Some(&both),
                "usdt-polygon-address",
                Some("10.08 usdt by usdt-polygon"),
            ),
            (
                &usd,
                Some(&both),
                "usdt-ethereum-address",
                Some("10 usdt by usdt"),
            ),
            (
                &usdt,
                Some(&polygon),
                "usdt-polygon-address",
                Some("10.08 usdt by usdt-polygon"),
            ),
            // The requested asset that no rate prices, with or without terms.
            (
                &usdt,
                Some(&polygon),
                "usdt-ethereum-address",
                Some("10 usdt"),
            ),
            (&usdt, None, "usdt-ethereum-address", Some("10 usdt")),
            // Without terms, another asset, or one that can't be told: only by hand.
            (&usd, None, "usdt-polygon-address", Some("by hand only")),
            (&usd, None, "eur-sepa-iban", Some("by hand only")),
            (&usd, None, "custom", Some("by hand only")),
            (&usd, None, "usd-ach", Some("by hand only")),
            // Under terms, another asset no rate prices, or one that can't be told.
            (
                &usd,
                Some(&polygon),
                "usdt-ethereum-address",
                Some("unusable"),
            ),
            (&usd, Some(&both), "eur-sepa-iban", Some("unusable")),
            (&usd, Some(&both), "usdt-polygon", Some("unusable")),
            // Malformed rates are refused, never taken as no rate or as the first match.
            (&usd, Some(&twice), "usdt-polygon-address", None),
            (&usd, Some(&twice_wide), "usdt-polygon-address", None),
            (&usd, Some(&empty), "usdt-polygon-address", None),
            (&usdt, Some(&empty), "usdt-ethereum-address", None),
        ];
        for (requested, rates, endpoint, expected) in cases {
            let got = pay(requested, rates, endpoint, 6);
            match expected {
                Some(expected) => assert_eq!(got.unwrap(), expected, "{endpoint}"),
                None => assert!(
                    matches!(got, Err(Error::Validation(_))),
                    "{endpoint}: {got:?}"
                ),
            }
        }
    }

    #[test]
    fn test_rate_for_endpoint_refuses_malformed_rates() {
        let endpoint = PaymentEndpointIdentifier::parse("usdt-polygon-address").unwrap();
        let both = rates(&[("usdt-polygon", "1.008"), ("usdt", "1")]);
        assert_eq!(rate_for_endpoint(&both, &endpoint).unwrap(), Some(&both[0]));
        let custom = PaymentEndpointIdentifier::parse("custom").unwrap();
        assert_eq!(rate_for_endpoint(&both, &custom).unwrap(), None);
        for list in [
            Vec::new(),
            rates(&[("usdt", "1"), ("usdt", "1")]),
            rates(&[("btc", "1"), ("btc", "2")]),
        ] {
            assert!(matches!(
                rate_for_endpoint(&list, &endpoint),
                Err(Error::Validation(_))
            ));
        }
    }
}
