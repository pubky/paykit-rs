//! Pure eligibility math over caller-provided complete accounting evidence.

use super::{
    allowance_period_window,
    amounts::{add_decimals, compare_decimals},
    AllowanceEvaluation, AllowanceEvaluationBlock, AllowanceEvaluationInput, AllowanceTerms,
};
use crate::{validation::parse_utc_timestamp, PaymentEndpointIdentifier, PaymentRequestTerms};
use chrono::{DateTime, Utc};

/// Match immutable request terms against an Allowance without lifecycle or time checks.
///
/// Exact case-sensitive assets, inclusive decimal amount range, and endpoint
/// intersection apply. This never selects an Allowance or resolves endpoint data.
pub fn match_allowance_request(
    terms: &AllowanceTerms,
    request: &PaymentRequestTerms,
) -> std::result::Result<Vec<PaymentEndpointIdentifier>, AllowanceEvaluationBlock> {
    request
        .validate()
        .map_err(|_| AllowanceEvaluationBlock::InvalidRequest)?;
    if request.amount().asset() != terms.asset() {
        return Err(AllowanceEvaluationBlock::AssetMismatch);
    }
    if let Some(range) = terms.per_payment_amount() {
        if compare_decimals(request.amount().value(), range.minimum()).is_lt()
            || compare_decimals(request.amount().value(), range.maximum()).is_gt()
        {
            return Err(AllowanceEvaluationBlock::AmountOutsideRange);
        }
    }
    let mut endpoints = Vec::new();
    for endpoint in request.accepted_payment_endpoint_identifiers() {
        if terms
            .allowed_payment_endpoint_identifiers()
            .is_none_or(|allowed| allowed.contains(endpoint))
            && !endpoints.contains(endpoint)
        {
            endpoints.push(endpoint.clone());
        }
    }
    if endpoints.is_empty() {
        return Err(AllowanceEvaluationBlock::NoEligibleEndpoint);
    }
    Ok(endpoints)
}

/// Check static matching, active time, watermark, and every amount/count limit.
///
/// The candidate consumes one count and its requested amount at `trusted_time`.
/// The SDK must atomically recheck lifecycle/scope, payment dedupe, this result,
/// and capacity before admitting payment, then durably advance the watermark.
/// No clock is read, state changed, endpoint resolved, or payment authorized here.
pub fn evaluate_allowance(
    input: &AllowanceEvaluationInput<'_>,
) -> std::result::Result<AllowanceEvaluation, AllowanceEvaluationBlock> {
    let endpoints = match_allowance_request(input.terms, input.request)?;
    check_allowance_time(input.terms, input.trusted_time, input.watermark)?;
    validate_usage(input)?;
    check_lifetime_limit(input)?;
    check_period_limits(input)?;
    Ok(AllowanceEvaluation {
        eligible_payment_endpoint_identifiers: endpoints,
    })
}

/// Check the active window and durable trusted-time watermark without consuming capacity.
///
/// This supports candidate selection and recurring Acceptance, which reserve no
/// usage. The caller remains responsible for lifecycle, history completeness,
/// consent, and persisting any watermark advancement under its storage rules.
pub fn check_allowance_time(
    terms: &AllowanceTerms,
    trusted_time: DateTime<Utc>,
    watermark: DateTime<Utc>,
) -> std::result::Result<(), AllowanceEvaluationBlock> {
    if trusted_time < watermark {
        return Err(AllowanceEvaluationBlock::ClockRollback);
    }
    if trusted_time.timestamp_subsec_nanos() >= 1_000_000_000
        || watermark.timestamp_subsec_nanos() >= 1_000_000_000
    {
        return Err(AllowanceEvaluationBlock::ArithmeticOverflow);
    }
    if parse_time_bound(terms.active_from())?.is_some_and(|start| trusted_time < start) {
        return Err(AllowanceEvaluationBlock::NotActive);
    }
    if parse_time_bound(terms.expires_at())?.is_some_and(|end| trusted_time >= end) {
        return Err(AllowanceEvaluationBlock::Expired);
    }
    Ok(())
}

fn parse_time_bound(
    bound: Option<&str>,
) -> std::result::Result<Option<chrono::DateTime<chrono::Utc>>, AllowanceEvaluationBlock> {
    bound
        .map(|value| {
            let time = parse_utc_timestamp(value, "Allowance time bound")
                .map_err(|_| AllowanceEvaluationBlock::ArithmeticOverflow)?;
            if time.timestamp_subsec_nanos() >= 1_000_000_000 {
                return Err(AllowanceEvaluationBlock::ArithmeticOverflow);
            }
            Ok(time.with_timezone(&chrono::Utc))
        })
        .transpose()
}

fn validate_usage(
    input: &AllowanceEvaluationInput<'_>,
) -> std::result::Result<(), AllowanceEvaluationBlock> {
    for entry in input.usage {
        if entry.admitted_at() > input.trusted_time {
            return Err(AllowanceEvaluationBlock::FutureUsage);
        }
        if entry.amount().asset() != input.terms.asset() {
            return Err(AllowanceEvaluationBlock::UsageAssetMismatch);
        }
    }
    Ok(())
}

fn check_lifetime_limit(
    input: &AllowanceEvaluationInput<'_>,
) -> std::result::Result<(), AllowanceEvaluationBlock> {
    if let Some(limit) = input.terms.lifetime_amount_limit() {
        let total = input
            .usage
            .iter()
            .fold(input.request.amount().value().to_owned(), |sum, entry| {
                add_decimals(&sum, entry.amount().value())
            });
        if compare_decimals(&total, limit).is_gt() {
            return Err(AllowanceEvaluationBlock::LifetimeAmountLimit);
        }
    }
    Ok(())
}

fn check_period_limits(
    input: &AllowanceEvaluationInput<'_>,
) -> std::result::Result<(), AllowanceEvaluationBlock> {
    for (index, limit) in input.terms.period_limits().iter().enumerate() {
        let window = allowance_period_window(limit.period(), input.trusted_time)?;
        let mut total = input.request.amount().value().to_owned();
        let mut count = 1_u64;
        for entry in input
            .usage
            .iter()
            .filter(|entry| window.contains(entry.admitted_at()))
        {
            total = add_decimals(&total, entry.amount().value());
            count = count
                .checked_add(1)
                .ok_or(AllowanceEvaluationBlock::ArithmeticOverflow)?;
        }
        if limit
            .amount_limit()
            .is_some_and(|amount| compare_decimals(&total, amount).is_gt())
        {
            return Err(AllowanceEvaluationBlock::PeriodAmountLimit { index });
        }
        if limit
            .payment_count_limit()
            .is_some_and(|maximum| count > maximum)
        {
            return Err(AllowanceEvaluationBlock::PeriodCountLimit { index });
        }
    }
    Ok(())
}
