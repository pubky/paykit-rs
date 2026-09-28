use std::{collections::HashSet, fmt};

use chrono::{DateTime, FixedOffset, SecondsFormat, TimeDelta};
use serde::{Deserialize, Serialize};

use crate::{
    validation::{parse_utc_timestamp, validate_asset_text, validate_decimal_text},
    EventId, PaykitError, PaymentEndpointIdentifier, PrivateMessageKind, Result,
};

use super::{BillingPeriod, PaymentProof, PaymentRequest, PaymentRequestId, PaymentRequestTerms};

/// Units of the payment asset owed per one unit of the requested asset.
/// Values are positive decimal strings; implementations must not use floating point.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionRate {
    /// Payment asset, using the same spelling as its endpoint identifier asset segment.
    pub asset: String,
    /// Payment asset units per requested asset unit.
    pub value: String,
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
        let deadline = match (self, period) {
            (Self::At { timestamp }, None) => parse_utc_timestamp(timestamp, "payment deadline")?,
            (Self::PeriodStart { seconds }, Some(period)) => {
                period.validate()?;
                deadline_after(&period.starts_at, *seconds)?
            }
            _ => {
                return Err(PaykitError::Validation(
                    "payment deadline must match request recurrence".into(),
                ))
            }
        };
        Ok(deadline.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}

fn deadline_after(starts_at: &str, seconds: u64) -> Result<DateTime<FixedOffset>> {
    let offset = i64::try_from(seconds)
        .ok()
        .and_then(TimeDelta::try_seconds)
        .ok_or_else(|| PaykitError::Validation("payment deadline offset is too large".into()))?;
    parse_utc_timestamp(starts_at, "period starts_at")?
        .checked_add_signed(offset)
        .ok_or_else(|| PaykitError::Validation("payment deadline is out of range".into()))
}

/// Payee-issued rates for one recurring Billing Period.
/// The Event ID is the quote identifier. New quotes never replace earlier quotes.
#[derive(Clone, Debug, PartialEq)]
pub struct PaymentConversionQuote {
    /// Protocol message version.
    pub version: u8,
    /// Private Message Kind.
    pub kind: PrivateMessageKind,
    /// Immutable quote identity, reused when referring to this quote from a proof.
    pub event_id: EventId,
    /// Request whose denomination and accepted endpoints apply.
    pub payment_request_id: PaymentRequestId,
    /// Period these rates apply to; eligibility is checked by the wallet's recurrence policy.
    pub billing_period: BillingPeriod,
    /// Payment asset units per one requested asset unit.
    pub rates: Vec<ConversionRate>,
    /// Inclusive payment deadline for this quote, in RFC3339 UTC.
    pub expires_at: String,
}

impl PaymentConversionQuote {
    /// Construct a recurring quote. Serialization and request correlation validate its fields.
    pub fn new(
        event_id: EventId,
        payment_request_id: PaymentRequestId,
        billing_period: BillingPeriod,
        rates: Vec<ConversionRate>,
        expires_at: String,
    ) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::PaymentConversionQuote,
            event_id,
            payment_request_id,
            billing_period,
            rates,
            expires_at,
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.version != 1 || self.kind != PrivateMessageKind::PaymentConversionQuote {
            return Err(PaykitError::Validation(
                "invalid Payment Conversion Quote header".into(),
            ));
        }
        self.billing_period.validate()?;
        validate_rates(&self.rates)?;
        parse_utc_timestamp(&self.expires_at, "quote expires_at")?;
        Ok(())
    }

    /// Check immutable request association. Role, lifecycle and calendar eligibility belong to the caller.
    pub fn validate_for_request(&self, request: &PaymentRequest) -> Result<()> {
        self.validate()?;
        if request.version != 1 || request.kind != PrivateMessageKind::PaymentRequest {
            return Err(PaykitError::Validation(
                "Payment Request must have version 1 and kind paykit.payment_request".into(),
            ));
        }
        request.request.validate()?;
        if self.payment_request_id != request.payment_request_id
            || request.request.conversion != Some(PaymentConversion::PerPeriod {})
        {
            return Err(PaykitError::Validation(
                "quote requires its recurring per-period Payment Request".into(),
            ));
        }
        validate_rate_assets(&self.rates, &request.request)
    }
}

fn validate_rates(rates: &[ConversionRate]) -> Result<()> {
    if rates.is_empty() {
        return Err(PaykitError::Validation(
            "conversion rates must not be empty".into(),
        ));
    }
    let mut assets = HashSet::new();
    for rate in rates {
        validate_decimal_text(&rate.value, "conversion rate.value")?;
        validate_asset_text(&rate.asset, "conversion rate asset")?;
        if !rate.value.bytes().any(|b| matches!(b, b'1'..=b'9')) || !assets.insert(&rate.asset) {
            return Err(PaykitError::Validation(
                "conversion rates must be positive and assets unique".into(),
            ));
        }
    }
    Ok(())
}

fn validate_rate_assets(rates: &[ConversionRate], terms: &PaymentRequestTerms) -> Result<()> {
    for rate in rates {
        if rate.asset == terms.amount.asset
            || !terms
                .accepted_payment_endpoint_identifiers
                .iter()
                .any(|id| endpoint_asset(id).ok() == Some(rate.asset.as_str()))
        {
            return Err(PaykitError::Validation(
                "conversion rate must name an accepted cross-asset endpoint".into(),
            ));
        }
    }
    Ok(())
}

fn endpoint_asset(identifier: &PaymentEndpointIdentifier) -> Result<&str> {
    identifier
        .as_str()
        .split_once('-')
        .map(|(asset, _)| asset)
        .filter(|asset| !asset.is_empty())
        .ok_or_else(|| {
            PaykitError::Validation(
                "conversion requires asset-prefixed endpoint identifiers".into(),
            )
        })
}

impl PaymentRequestTerms {
    pub(super) fn validate_conversion(&self) -> Result<()> {
        if let Some(conversion) = &self.conversion {
            for id in &self.accepted_payment_endpoint_identifiers {
                endpoint_asset(id)?;
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
            match (deadline, &self.recurrence) {
                (PaymentDeadline::At { .. }, None) => {
                    deadline.at(None)?;
                }
                (PaymentDeadline::PeriodStart { seconds }, Some(recurrence)) => {
                    deadline_after(&recurrence.starts_at, *seconds)?;
                }
                _ => {
                    return Err(PaykitError::Validation(
                        "payment deadline must match request recurrence".into(),
                    ))
                }
            }
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
            (Some(id), Some(quote)) if *id == quote.event_id => {
                quote.validate_for_request(request)?;
                if self.billing_period.as_ref() != Some(&quote.billing_period) {
                    return Err(PaykitError::Validation(
                        "proof and quote Billing Period must match".into(),
                    ));
                }
                self.validate_rate_coverage(&request.request, &quote.rates)
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
                if endpoint_asset(&self.payment_endpoint_identifier)? != terms.amount.asset
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
        let asset = endpoint_asset(&self.payment_endpoint_identifier)?;
        if asset == terms.amount.asset || rates.iter().any(|rate| rate.asset == asset) {
            Ok(())
        } else {
            Err(PaykitError::Validation(
                "payment asset has no agreed conversion rate".into(),
            ))
        }
    }
}
