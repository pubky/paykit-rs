//! Pricing a payment by conversion rates (Paykit spec 7.2 step 1, 8.2, Paykit wire 4.4, 4.9).
//!
//! The amount to an endpoint is computed before paying, as in v0.1
//! (`specs/payment-conversion.md`, "Conversion terms"), by the one selection
//! [`paykit_lib::amount_for_endpoint`], with the endpoint's smallest unit named by the payment
//! method ([`crate::Rail::fraction_digits`]): to an endpoint of the requested asset that no
//! rate prices, the request's amount, rounded up once to that unit (Paykit wire 1.2); to an
//! endpoint a rate prices, the request's amount times the asset-rail rate if there is one,
//! else the asset-wide one, rounded up once to that unit. Under conversion terms, an endpoint
//! of another asset that no rate prices is unusable. A request without conversion terms
//! promises no rate: an endpoint of another asset is paid only by hand, on its user's
//! confirmation of what the method would transfer ([`Priced::ByHand`]).
//!
//! A payment priced by a rate is made only by hand: its user confirms the amount so
//! computed ([`PricedPayment`]), and the payer pays exactly that. The payee expects the amount
//! computed by the same rule for the endpoint its proof names.

use paykit_lib::bodies::Conversion;
use paykit_lib::{
    amount_for_endpoint, EndpointAmount, PaymentAmount, PaymentEndpointIdentifier, Rate,
};

use crate::rail::Rail;
use crate::requests::RequestRef;

/// Where the rates pricing a payment come from (Paykit spec 7.2 step 1, 8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RateSource {
    /// The request's fixed rates: conversion terms `fixed` (Paykit wire 4.4).
    Request,
    /// The rates of a Billing Period's period details, under conversion terms `per_period`
    /// (Paykit wire 4.9).
    PeriodDetails {
        /// The period details' `msg_id`.
        msg_id: String,
    },
}

/// What a payment priced by a rate shows its user before paying (Paykit spec 7.2 step 1):
/// the request, the destination, named by its payee app, Payment Endpoint Identifier and a
/// hash of its payload, never the payload itself, the amount requested, the rate used and
/// where it comes from, and the amount so computed, in the endpoint's asset. A
/// [`crate::Refusal::PricedByRate`] carries it, and a payment the user then makes carries it
/// back ([`crate::PayOptions::with_rate_confirmed`]): the SDK computes the amount again and
/// pays only when what it would show now is this, field for field, and then pays exactly
/// [`PricedPayment::amount`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PricedPayment {
    /// The request paid.
    pub request: RequestRef,
    /// The payee app whose destination it is.
    pub app_id: [u8; 16],
    /// The Payment Endpoint Identifier.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// The hash of the Payment Endpoint Payload, which names it without showing it.
    pub payload_hash: [u8; 32],
    /// The amount requested, in the requested asset.
    pub requested: PaymentAmount,
    /// Where the rate comes from.
    pub source: RateSource,
    /// The rate used: the asset-rail rate if there is one, else the asset-wide one.
    pub rate: Rate,
    /// The amount paid: the amount requested times the rate, rounded up once to the
    /// endpoint's smallest unit, in the endpoint's asset.
    pub amount: PaymentAmount,
}

/// The amount a payment pays to an endpoint ([`price`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Priced {
    /// Of the requested asset, and no rate prices it: the amount requested, rounded up once
    /// to the endpoint's smallest unit.
    Plain(PaymentAmount),
    /// Priced by `rate`: `amount`, in the endpoint's asset.
    ByRate {
        /// The rate used.
        rate: Rate,
        /// The amount computed.
        amount: PaymentAmount,
    },
    /// A request without conversion terms, to an endpoint of another asset, or of one that
    /// can't be told: the amount requested, as given, paid only by hand, once its user has
    /// confirmed what the method would transfer for it. An automatic payment never pays it.
    ByHand(PaymentAmount),
    /// Unusable for the payment: under conversion terms, of another asset that no rate
    /// prices or of no asset that can be told; or of an endpoint whose smallest unit the
    /// method can't name; or an amount that doesn't fit.
    Unusable(&'static str),
}

/// Why a payment of a request without conversion terms to an endpoint of another asset is
/// made only by hand (Paykit spec 7.2 step 1).
pub(crate) const BY_HAND_ONLY: &str = "Paykit spec 7.2 step 1: a request without conversion \
     terms promises no rate, so an endpoint of another asset is paid only by hand, on its \
     user's confirmation of what the method would transfer";

/// The fixed rates of conversion terms `conversion`: the rates of a request whose
/// conversion terms are `fixed`, `None` for none or `per_period`.
pub(crate) fn fixed_rates(conversion: Option<&Conversion>) -> Option<&[Rate]> {
    match conversion {
        Some(Conversion::Fixed(rates)) => Some(rates),
        _ => None,
    }
}

/// The amount a payment of a request for `requested` pays to `endpoint_id` (Paykit spec 7.2
/// step 1), as payer and as payee alike, by [`amount_for_endpoint`] with the smallest unit
/// `rail` names for the endpoint: `rates` are the conversion terms' rates, the request's
/// fixed rates or the period details' rates, `None` for a request without conversion terms.
/// An endpoint whose unit the method can't name is [`Priced::Unusable`], whatever the terms.
pub(crate) fn price(
    rail: &dyn Rail,
    requested: &PaymentAmount,
    rates: Option<&[Rate]>,
    endpoint_id: &PaymentEndpointIdentifier,
) -> Priced {
    let Some(digits) = rail.fraction_digits(endpoint_id) else {
        return Priced::Unusable(
            "Paykit spec 7.2 step 1: the payment method names no smallest unit for the endpoint",
        );
    };
    match amount_for_endpoint(requested, rates, endpoint_id, digits) {
        Ok(EndpointAmount::ByRate { rate, amount }) => Priced::ByRate { rate, amount },
        Ok(EndpointAmount::RequestedAsset(amount)) => Priced::Plain(amount),
        Ok(EndpointAmount::ByHandOnly) => Priced::ByHand(requested.clone()),
        Ok(_) => Priced::Unusable(
            "Paykit spec 7.2 step 1: under conversion terms an endpoint of another asset that no rate prices, or whose asset can't be told, is unusable",
        ),
        Err(_) => Priced::Unusable(
            "Paykit wire 1.2, 4.4: the rates are malformed, the method's smallest unit is out of bounds, or the amount doesn't fit",
        ),
    }
}

#[cfg(test)]
mod tests {
    use paykit_lib::{Asset, Decimal};

    use super::*;
    use crate::rail::FakeRail;

    fn amount(value: &str, asset: &str) -> PaymentAmount {
        PaymentAmount {
            value: Decimal::new(value).unwrap(),
            asset: Asset::new(asset).unwrap(),
        }
    }

    fn rate(asset: &str, value: &str) -> Rate {
        Rate::new(Asset::new(asset).unwrap(), Decimal::new(value).unwrap()).unwrap()
    }

    fn id(text: &str) -> PaymentEndpointIdentifier {
        PaymentEndpointIdentifier::new(text).unwrap()
    }

    #[test]
    fn test_conversion_price_by_the_asset_rail_rate_first() {
        let rail = FakeRail::new();
        let usd = amount("0.05", "usd");
        let rates = [rate("btc", "0.000012345"), rate("btc-lightning", "0.00002")];
        // 61.725 satoshis by the asset-wide rate, rounded up to 62.
        assert_eq!(
            price(&rail, &usd, Some(&rates), &id("btc-bitcoin-p2tr")),
            Priced::ByRate {
                rate: rates[0].clone(),
                amount: amount("0.00000062", "btc"),
            }
        );
        // The asset-rail rate prices its own rail.
        assert_eq!(
            price(&rail, &usd, Some(&rates), &id("btc-lightning-bolt11")),
            Priced::ByRate {
                rate: rates[1].clone(),
                amount: amount("0.000001", "btc"),
            }
        );
    }

    #[test]
    fn test_conversion_price_plain_without_terms_or_for_the_requested_asset() {
        let rail = FakeRail::new();
        let usd = amount("0.05", "usd");
        // Another asset without terms: the amount as given, only by hand.
        assert_eq!(
            price(&rail, &usd, None, &id("eur-sepa-iban")),
            Priced::ByHand(usd.clone())
        );
        // The requested asset with the method's default unit: no rounding needed.
        let rates = [rate("btc", "0.00001")];
        let ach = id("usd-ach-account");
        assert_eq!(
            price(&rail, &usd, Some(&rates), &ach),
            Priced::Plain(usd.clone())
        );
        // Its unit named: rounded up once to it, with or without terms.
        rail.set_fraction_digits(ach.clone(), Some(1));
        assert_eq!(
            price(&rail, &usd, Some(&rates), &ach),
            Priced::Plain(amount("0.1", "usd"))
        );
        let fine = amount("0.000000001", "btc");
        assert_eq!(
            price(&rail, &fine, None, &id("btc-bitcoin-p2tr")),
            Priced::Plain(amount("0.00000001", "btc"))
        );
    }

    #[test]
    fn test_conversion_price_unusable_for_an_unpriced_asset_or_an_unknown_unit() {
        let rail = FakeRail::new();
        let usd = amount("0.05", "usd");
        let rates = [rate("btc", "0.00001")];
        assert!(matches!(
            price(&rail, &usd, Some(&rates), &id("eur-sepa-iban")),
            Priced::Unusable(_)
        ));
        assert!(matches!(
            price(&rail, &usd, Some(&rates), &id("custom")),
            Priced::Unusable(_)
        ));
        rail.set_fraction_digits(id("btc-bitcoin-p2tr"), None);
        assert!(matches!(
            price(&rail, &usd, Some(&rates), &id("btc-bitcoin-p2tr")),
            Priced::Unusable(_)
        ));
        rail.set_fraction_digits(id("btc-bitcoin-p2tr"), Some(19));
        assert!(matches!(
            price(&rail, &usd, Some(&rates), &id("btc-bitcoin-p2tr")),
            Priced::Unusable(_)
        ));
    }

    #[test]
    fn test_conversion_plain_request_requires_every_endpoints_unit() {
        let rail = FakeRail::new();
        let requested = amount("1", "usd");
        for endpoint in [id("eur-sepa-iban"), id("custom")] {
            assert!(matches!(
                price(&rail, &requested, None, &endpoint),
                Priced::ByHand(_)
            ));
            for digits in [None, Some(19)] {
                rail.set_fraction_digits(endpoint.clone(), digits);
                assert!(matches!(
                    price(&rail, &requested, None, &endpoint),
                    Priced::Unusable(_)
                ));
            }
        }
    }

    #[test]
    fn test_conversion_fixed_rates_only_of_fixed_terms() {
        let rates = vec![rate("btc", "0.00001")];
        let fixed = Conversion::Fixed(rates.clone());
        assert_eq!(fixed_rates(Some(&fixed)), Some(rates.as_slice()));
        assert_eq!(fixed_rates(Some(&Conversion::PerPeriod)), None);
        assert_eq!(fixed_rates(None), None);
    }
}
