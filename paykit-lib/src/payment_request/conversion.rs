use std::{collections::HashSet, fmt};

use chrono::{DateTime, Datelike, FixedOffset, SecondsFormat, TimeDelta};
use serde::{Deserialize, Serialize};

use crate::{
    validation::{parse_utc_timestamp, validate_decimal_text, validate_outgoing_version_kind},
    EventId, PaykitError, PaymentEndpointIdentifier, PrivateMessageKind, Result,
};

use super::{BillingPeriod, PaymentProof, PaymentRequest, PaymentRequestId, PaymentRequestTerms};

/// Units of the payment asset owed per one unit of the requested asset.
/// Values are positive decimal strings; implementations must not use floating point.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionRate {
    /// Payment asset or asset-rail selector, for example `usdt` or `usdt-polygon`.
    pub asset: String,
    /// Payment asset units per requested asset unit.
    pub value: String,
}

impl ConversionRate {
    /// Select an explicit rate, preferring an exact asset-rail match over the asset default.
    ///
    /// Validates the nonempty rate list and endpoint grammar. This does not check
    /// request acceptance or quote validity. `None` means no explicit rate: only
    /// the requested asset may then use implicit 1:1 pricing.
    pub fn for_endpoint<'a>(
        rates: &'a [Self],
        endpoint: &PaymentEndpointIdentifier,
    ) -> Result<Option<&'a Self>> {
        validate_rates(rates)?;
        let (asset, asset_rail) = endpoint_rate_selectors(endpoint)?;
        Ok(rates
            .iter()
            .find(|rate| rate.asset == asset_rail)
            .or_else(|| rates.iter().find(|rate| rate.asset == asset)))
    }
}

impl fmt::Debug for ConversionRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConversionRate(<redacted>)")
    }
}

/// Conversion policy agreed in immutable Payment Request terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PaymentConversion {
    /// Fixed rates apply to the request and every recurring installment.
    Fixed {
        /// An omitted payment asset is unavailable for conversion.
        rates: Vec<ConversionRate>,
    },
    /// Recurring cross-asset payments require a payee-issued quote for their Billing Period.
    PerPeriod {},
}

/// Payment deadline, independent of the deadline for accepting a proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PaymentDeadline {
    /// Absolute deadline for a one-time payment.
    At {
        /// RFC3339 UTC timestamp with a Z suffix.
        timestamp: String,
    },
    /// Deadline measured from the start of each recurring Billing Period.
    PeriodStart {
        /// Nonnegative elapsed seconds after the period starts.
        seconds: u64,
    },
}

impl PaymentDeadline {
    /// Resolve a deadline. Callers verify actual payment time, not proof arrival time.
    pub fn at(&self, period: Option<&BillingPeriod>) -> Result<String> {
        if let Some(period) = period {
            period.validate()?;
        }
        let deadline = self.resolve(period.map(|period| period.starts_at.as_str()))?;
        Ok(deadline.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }

    fn resolve(&self, period_start: Option<&str>) -> Result<DateTime<FixedOffset>> {
        match (self, period_start) {
            (Self::At { timestamp }, None) => conversion_timestamp(timestamp, "payment deadline"),
            (Self::PeriodStart { seconds }, Some(start)) => deadline_after(start, *seconds),
            _ => Err(PaykitError::Validation(
                "payment deadline must match request recurrence".into(),
            )),
        }
    }
}

fn conversion_timestamp(value: &str, field: &str) -> Result<DateTime<FixedOffset>> {
    let parsed = parse_utc_timestamp(value, field)?;
    if value.as_bytes().get(10) != Some(&b'T')
        || !(0..=9999).contains(&parsed.year())
        || parsed.timestamp_subsec_nanos() >= 1_000_000_000
    {
        return Err(PaykitError::Validation(format!(
            "{field} must use a four-digit year, T separator and no leap second"
        )));
    }
    Ok(parsed)
}

fn deadline_after(starts_at: &str, seconds: u64) -> Result<DateTime<FixedOffset>> {
    let offset = i64::try_from(seconds)
        .ok()
        .and_then(TimeDelta::try_seconds)
        .ok_or_else(|| PaykitError::Validation("payment deadline offset is too large".into()))?;
    conversion_timestamp(starts_at, "period starts_at")?
        .checked_add_signed(offset)
        .filter(|deadline| (0..=9999).contains(&deadline.year()))
        .ok_or_else(|| PaykitError::Validation("payment deadline is out of range".into()))
}

/// Payee-issued rates for one recurring Billing Period.
/// The Event ID is the quote identifier. New quotes never replace earlier quotes.
///
/// Fields cannot be mutated after validated construction.
///
/// ```compile_fail,E0616
/// fn cannot_mutate(mut quote: paykit_lib::PaymentConversionQuote) {
///     quote.expires_at = "invalid".into();
/// }
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct PaymentConversionQuote {
    /// Protocol message version.
    version: u8,
    /// Private Message Kind.
    kind: PrivateMessageKind,
    /// Immutable quote identity, reused when referring to this quote from a proof.
    event_id: EventId,
    /// Request whose denomination and accepted endpoints apply.
    payment_request_id: PaymentRequestId,
    /// Period these rates apply to; eligibility is checked by the wallet's recurrence policy.
    billing_period: BillingPeriod,
    /// Payment asset units per one requested asset unit.
    rates: Vec<ConversionRate>,
    /// Inclusive start of the payment validity interval, in RFC3339 UTC.
    valid_from: String,
    /// Inclusive payment deadline for this quote, in RFC3339 UTC.
    expires_at: String,
}

impl PaymentConversionQuote {
    /// Construct and validate a recurring quote.
    pub fn new(
        event_id: EventId,
        payment_request_id: PaymentRequestId,
        billing_period: BillingPeriod,
        rates: Vec<ConversionRate>,
        valid_from: String,
        expires_at: String,
    ) -> Result<Self> {
        let quote = Self {
            version: 1,
            kind: PrivateMessageKind::PaymentConversionQuote,
            event_id,
            payment_request_id,
            billing_period,
            rates,
            valid_from,
            expires_at,
        };
        quote.validate()?;
        Ok(quote)
    }

    /// Return the protocol message version.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Return the Private Message Kind.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }

    /// Access the immutable quote Event ID.
    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }

    /// Access the associated Payment Request ID.
    pub fn payment_request_id(&self) -> &PaymentRequestId {
        &self.payment_request_id
    }

    /// Access the quoted Billing Period.
    pub fn billing_period(&self) -> &BillingPeriod {
        &self.billing_period
    }

    /// Access the immutable conversion rates.
    pub fn rates(&self) -> &[ConversionRate] {
        &self.rates
    }

    /// Access the inclusive start of the quote validity interval.
    pub fn valid_from(&self) -> &str {
        &self.valid_from
    }

    /// Access the inclusive end of the quote validity interval.
    pub fn expires_at(&self) -> &str {
        &self.expires_at
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_outgoing_version_kind(
            self.version,
            self.kind,
            PrivateMessageKind::PaymentConversionQuote,
            "Payment Conversion Quote",
        )?;
        self.billing_period.validate()?;
        validate_rates(&self.rates)?;
        conversion_timestamp(&self.billing_period.starts_at, "period starts_at")?;
        conversion_timestamp(&self.billing_period.ends_at, "period ends_at")?;
        let start = conversion_timestamp(&self.valid_from, "quote valid_from")?;
        let end = conversion_timestamp(&self.expires_at, "quote expires_at")?;
        if start > end {
            return Err(PaykitError::Validation(
                "quote validity interval is reversed".into(),
            ));
        }
        Ok(())
    }

    /// Check immutable request association. Role, lifecycle and calendar eligibility belong to the caller.
    pub fn validate_for_request(&self, request: &PaymentRequest) -> Result<()> {
        self.validate()?;
        request.validate()?;
        if self.payment_request_id != request.payment_request_id
            || request.request.conversion != Some(PaymentConversion::PerPeriod {})
        {
            return Err(PaykitError::Validation(
                "quote requires its recurring per-period Payment Request".into(),
            ));
        }
        validate_rate_assets(&self.rates, &request.request)?;
        // Same-asset per-period payments use implicit 1:1 pricing without a quote,
        // so an optional quote cannot change that price.
        if self
            .rates
            .iter()
            .any(|rate| rate.asset.split('-').next() == Some(request.request.amount.asset()))
        {
            return Err(PaykitError::Validation(
                "per-period quotes only price cross-asset payments".into(),
            ));
        }
        Ok(())
    }
}

fn validate_rates(rates: &[ConversionRate]) -> Result<()> {
    if rates.is_empty() {
        return Err(PaykitError::Validation(
            "conversion rates must not be empty".into(),
        ));
    }
    let mut selectors = HashSet::new();
    for rate in rates {
        validate_decimal_text(&rate.value, "conversion rate.value")?;
        let mut segments = rate.asset.split('-');
        if !segments.next().is_some_and(valid_asset_segment)
            || !segments.next().is_none_or(valid_asset_segment)
            || segments.next().is_some()
        {
            return Err(PaykitError::Validation(
                "conversion rate asset must be asset or asset-rail with lowercase alphanumeric segments".into(),
            ));
        }
        if !rate.value.bytes().any(|b| matches!(b, b'1'..=b'9')) || !selectors.insert(&rate.asset) {
            return Err(PaykitError::Validation(
                "conversion rates must be positive and selectors unique".into(),
            ));
        }
    }
    Ok(())
}

fn validate_rate_assets(rates: &[ConversionRate], terms: &PaymentRequestTerms) -> Result<()> {
    for rate in rates {
        if !terms
            .accepted_payment_endpoint_identifiers
            .iter()
            .any(|id| {
                endpoint_rate_selectors(id).is_ok_and(|(asset, asset_rail)| {
                    rate.asset == asset || rate.asset == asset_rail
                })
            })
        {
            return Err(PaykitError::Validation(
                "conversion rate must name an accepted asset or asset-rail".into(),
            ));
        }
    }
    Ok(())
}

fn endpoint_rate_selectors(identifier: &PaymentEndpointIdentifier) -> Result<(&str, &str)> {
    let (asset_rail, format) = identifier.as_str().rsplit_once('-').unwrap_or(("", ""));
    let (asset, rail) = asset_rail.split_once('-').unwrap_or(("", ""));
    if valid_asset_segment(asset) && valid_asset_segment(rail) && valid_asset_segment(format) {
        Ok((asset, asset_rail))
    } else {
        Err(PaykitError::Validation(
            "conversion requires asset-rail-format identifiers with lowercase alphanumeric segments".into(),
        ))
    }
}

fn valid_asset_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

impl PaymentRequestTerms {
    pub(super) fn validate_conversion(&self) -> Result<()> {
        if let Some(conversion) = &self.conversion {
            if !valid_asset_segment(self.amount.asset()) {
                return Err(PaykitError::Validation(
                    "conversion requires a lowercase alphanumeric request asset".into(),
                ));
            }
            for id in &self.accepted_payment_endpoint_identifiers {
                endpoint_rate_selectors(id)?;
            }
            match conversion {
                PaymentConversion::Fixed { rates } => {
                    validate_rates(rates)?;
                    validate_rate_assets(rates, self)?;
                }
                PaymentConversion::PerPeriod {} if self.recurrence.is_none() => {
                    return Err(PaykitError::Validation(
                        "per-period conversion requires recurrence".into(),
                    ))
                }
                PaymentConversion::PerPeriod {} => {}
            }
        }
        if let Some(deadline) = &self.payment_deadline {
            deadline.resolve(
                self.recurrence
                    .as_ref()
                    .map(|recurrence| recurrence.starts_at.as_str()),
            )?;
        }
        Ok(())
    }
}

impl PaymentProof {
    /// Validate quote selection without rejecting a delayed proof merely because the quote has expired.
    /// Callers separately verify the paid amount and actual payment time against the selected terms.
    pub fn validate_conversion_quote(
        &self,
        request: &PaymentRequest,
        quote: Option<&PaymentConversionQuote>,
    ) -> Result<()> {
        self.validate_for_request(request)?;
        match (&self.conversion_quote_id, quote) {
            (Some(id), Some(quote)) if id == quote.event_id() => {
                quote.validate_for_request(request)?;
                let same_period = self
                    .billing_period
                    .as_ref()
                    .map(|period| -> Result<bool> {
                        Ok(conversion_timestamp(&period.starts_at, "period starts_at")?
                            == conversion_timestamp(
                                quote.billing_period().starts_at(),
                                "period starts_at",
                            )?
                            && conversion_timestamp(&period.ends_at, "period ends_at")?
                                == conversion_timestamp(
                                    quote.billing_period().ends_at(),
                                    "period ends_at",
                                )?)
                    })
                    .transpose()?
                    .unwrap_or(false);
                if !same_period {
                    return Err(PaykitError::Validation(
                        "proof and quote Billing Period must match".into(),
                    ));
                }
                self.validate_rate_coverage(&request.request, quote.rates())
            }
            (None, None) => Ok(()),
            _ => Err(PaykitError::Validation(
                "proof must reference a known matching conversion quote".into(),
            )),
        }
    }

    pub(super) fn validate_conversion(&self, terms: &PaymentRequestTerms) -> Result<()> {
        match &terms.conversion {
            Some(PaymentConversion::Fixed { rates }) => {
                if self.conversion_quote_id.is_some() {
                    return Err(PaykitError::Validation(
                        "fixed conversion does not use quotes".into(),
                    ));
                }
                self.validate_rate_coverage(terms, rates)
            }
            Some(PaymentConversion::PerPeriod {}) => {
                if endpoint_rate_selectors(&self.payment_endpoint_identifier)?.0
                    != terms.amount.asset()
                    && self.conversion_quote_id.is_none()
                {
                    return Err(PaykitError::Validation(
                        "cross-asset proof requires a conversion quote".into(),
                    ));
                }
                Ok(())
            }
            None if self.conversion_quote_id.is_some() => Err(PaykitError::Validation(
                "request does not accept conversion quotes".into(),
            )),
            None => Ok(()),
        }
    }

    fn validate_rate_coverage(
        &self,
        terms: &PaymentRequestTerms,
        rates: &[ConversionRate],
    ) -> Result<()> {
        let asset = endpoint_rate_selectors(&self.payment_endpoint_identifier)?.0;
        if ConversionRate::for_endpoint(rates, &self.payment_endpoint_identifier)?.is_some()
            || asset == terms.amount.asset()
        {
            Ok(())
        } else {
            Err(PaykitError::Validation(
                "payment asset has no agreed conversion rate".into(),
            ))
        }
    }
}
