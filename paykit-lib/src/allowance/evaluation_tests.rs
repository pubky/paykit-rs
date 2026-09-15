use chrono::{DateTime, Utc};

use super::*;
use crate::{PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestTerms};

fn time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

fn endpoint(value: &str) -> PaymentEndpointIdentifier {
    PaymentEndpointIdentifier::new(value).unwrap()
}

fn request(value: &str, asset: &str) -> PaymentRequestTerms {
    PaymentRequestTerms::builder(
        PaymentAmount::new(value, asset).unwrap(),
        PaymentReference::new("invoice-1").unwrap(),
        vec![
            endpoint("btc-lightning-bolt11"),
            endpoint("btc-lightning-bolt12"),
        ],
    )
    .build()
    .unwrap()
}

fn usage(value: &str, admitted_at: DateTime<Utc>) -> AllowanceUsageEntry {
    AllowanceUsageEntry::new(PaymentAmount::new(value, "btc").unwrap(), admitted_at).unwrap()
}

fn evaluate(
    terms: &AllowanceTerms,
    value: &str,
    now: DateTime<Utc>,
    usage: &[AllowanceUsageEntry],
) -> std::result::Result<AllowanceEvaluation, AllowanceEvaluationBlock> {
    evaluate_allowance(&AllowanceEvaluationInput {
        terms,
        request: &request(value, "btc"),
        trusted_time: now,
        watermark: now,
        usage,
    })
}

#[test]
fn test_static_matching_requires_exact_asset_and_inclusive_amount() {
    let terms = AllowanceTerms::builder("btc")
        .per_payment_amount(AllowanceAmountRange::new(".5", "1.00").unwrap())
        .build()
        .unwrap();
    for value in ["0.500", "01."] {
        assert!(match_allowance_request(&terms, &request(value, "btc")).is_ok());
    }
    for value in [".4999999999999999999999", "1.000000000000000000001"] {
        assert_eq!(
            match_allowance_request(&terms, &request(value, "btc")),
            Err(AllowanceEvaluationBlock::AmountOutsideRange)
        );
    }
    assert_eq!(
        match_allowance_request(&terms, &request("1", "BTC")),
        Err(AllowanceEvaluationBlock::AssetMismatch)
    );
}

#[test]
fn test_static_matching_intersects_endpoint_identifiers() {
    let terms = AllowanceTerms::builder("btc")
        .lifetime_amount_limit("10")
        .allowed_payment_endpoint_identifiers(vec![endpoint("btc-lightning-bolt12")])
        .build()
        .unwrap();
    assert_eq!(
        match_allowance_request(&terms, &request("1", "btc")).unwrap(),
        vec![endpoint("btc-lightning-bolt12")]
    );
    let terms = AllowanceTerms::builder("btc")
        .lifetime_amount_limit("10")
        .allowed_payment_endpoint_identifiers(vec![endpoint("btc-onchain")])
        .build()
        .unwrap();
    assert_eq!(
        match_allowance_request(&terms, &request("1", "btc")),
        Err(AllowanceEvaluationBlock::NoEligibleEndpoint)
    );
}

#[test]
fn test_active_window_includes_start_and_excludes_expiry() {
    let terms = AllowanceTerms::builder("btc")
        .active_from("2026-01-01T00:00:00Z")
        .expires_at("2026-02-01T00:00:00Z")
        .build()
        .unwrap();
    assert_eq!(
        evaluate(&terms, "1", time("2025-12-31T23:59:59Z"), &[]),
        Err(AllowanceEvaluationBlock::NotActive)
    );
    assert!(evaluate(&terms, "1", time("2026-01-01T00:00:00Z"), &[]).is_ok());
    assert_eq!(
        evaluate(&terms, "1", time("2026-02-01T00:00:00Z"), &[]),
        Err(AllowanceEvaluationBlock::Expired)
    );
}

#[test]
fn test_candidate_checks_do_not_require_current_capacity() {
    let terms = AllowanceTerms::builder("btc")
        .lifetime_amount_limit("0")
        .build()
        .unwrap();
    let now = time("2026-01-01T00:00:00Z");
    assert!(match_allowance_request(&terms, &request("1", "btc")).is_ok());
    assert!(check_allowance_time(&terms, now, now).is_ok());
    assert_eq!(
        evaluate(&terms, "1", now, &[]),
        Err(AllowanceEvaluationBlock::LifetimeAmountLimit)
    );
}

#[test]
fn test_rolling_window_excludes_lower_boundary_and_includes_now() {
    let now = time("2026-01-02T00:00:00Z");
    let period = AllowancePeriod::rolling(1, AllowancePeriodUnit::Day).unwrap();
    let window = allowance_period_window(&period, now).unwrap();
    assert!(!window.contains(time("2026-01-01T00:00:00Z")));
    assert!(window.contains(time("2026-01-01T00:00:00.000000001Z")));
    assert!(window.contains(now));
    let terms = AllowanceTerms::builder("btc")
        .period_limits(vec![AllowancePeriodLimit::new(
            Some("2".into()),
            Some(2),
            period,
        )
        .unwrap()])
        .build()
        .unwrap();
    assert!(evaluate(
        &terms,
        "1",
        now,
        &[usage("100", window.starts_at()), usage("1", now)]
    )
    .is_ok());
    assert_eq!(
        evaluate(&terms, "1", now, &[usage("2", now)]),
        Err(AllowanceEvaluationBlock::PeriodAmountLimit { index: 0 })
    );
}

#[test]
fn test_every_period_and_lifetime_limit_applies() {
    let now = time("2026-01-02T00:00:00Z");
    let periods = vec![
        AllowancePeriodLimit::new(
            Some("10".into()),
            None,
            AllowancePeriod::rolling(1, AllowancePeriodUnit::Day).unwrap(),
        )
        .unwrap(),
        AllowancePeriodLimit::new(
            None,
            Some(1),
            AllowancePeriod::rolling(1, AllowancePeriodUnit::Week).unwrap(),
        )
        .unwrap(),
    ];
    let terms = AllowanceTerms::builder("btc")
        .period_limits(periods)
        .lifetime_amount_limit("100")
        .build()
        .unwrap();
    assert_eq!(
        evaluate(&terms, "1", now, &[usage("1", now)]),
        Err(AllowanceEvaluationBlock::PeriodCountLimit { index: 1 })
    );
    assert_eq!(
        evaluate(
            &terms,
            "1",
            now,
            &[usage("100", time("2025-01-01T00:00:00Z"))]
        ),
        Err(AllowanceEvaluationBlock::LifetimeAmountLimit)
    );
}

#[test]
fn test_zero_count_limit_blocks_even_zero_amount_candidate() {
    let terms = AllowanceTerms::builder("btc")
        .lifetime_amount_limit("10")
        .period_limits(vec![AllowancePeriodLimit::new(
            None,
            Some(0),
            AllowancePeriod::rolling(1, AllowancePeriodUnit::Day).unwrap(),
        )
        .unwrap()])
        .build()
        .unwrap();
    assert_eq!(
        evaluate(&terms, "0", time("2026-01-01T00:00:00Z"), &[]),
        Err(AllowanceEvaluationBlock::PeriodCountLimit { index: 0 })
    );
}

#[test]
fn test_usage_and_candidate_amounts_are_added_without_rounding() {
    let now = time("2026-01-01T00:00:00Z");
    let terms = AllowanceTerms::builder("btc")
        .lifetime_amount_limit("1.000000000000000000000000000001")
        .build()
        .unwrap();
    assert!(evaluate(
        &terms,
        ".000000000000000000000000000001",
        now,
        &[usage("1", now)]
    )
    .is_ok());
    assert_eq!(
        evaluate(
            &terms,
            ".000000000000000000000000000002",
            now,
            &[usage("1", now)]
        ),
        Err(AllowanceEvaluationBlock::LifetimeAmountLimit)
    );
}

#[test]
fn test_watermark_and_future_usage_block_capacity_reopening() {
    let terms = AllowanceTerms::builder("btc")
        .lifetime_amount_limit("100")
        .build()
        .unwrap();
    let request = request("1", "btc");
    let now = time("2026-01-01T09:00:00Z");
    let later = time("2026-01-01T10:00:00Z");
    let entries = [usage("1", later)];
    let mut input = AllowanceEvaluationInput {
        terms: &terms,
        request: &request,
        trusted_time: now,
        watermark: later,
        usage: &entries,
    };
    assert_eq!(
        evaluate_allowance(&input),
        Err(AllowanceEvaluationBlock::ClockRollback)
    );
    input.watermark = now;
    assert_eq!(
        evaluate_allowance(&input),
        Err(AllowanceEvaluationBlock::FutureUsage)
    );
    input.trusted_time = later;
    assert!(evaluate_allowance(&input).is_ok());
}

#[test]
fn test_usage_asset_mismatch_fails_closed_without_leaking_amount() {
    let terms = AllowanceTerms::builder("btc")
        .lifetime_amount_limit("100")
        .build()
        .unwrap();
    let now = time("2026-01-01T00:00:00Z");
    let entry =
        AllowanceUsageEntry::new(PaymentAmount::new("123.456", "BTC").unwrap(), now).unwrap();
    assert!(!format!("{entry:?}").contains("123.456"));
    assert_eq!(
        evaluate(&terms, "1", now, &[entry]),
        Err(AllowanceEvaluationBlock::UsageAssetMismatch)
    );
}

#[test]
fn test_calendar_period_capacity_does_not_reopen_when_clock_crosses_midnight_backwards() {
    let terms = AllowanceTerms::builder("btc")
        .period_limits(vec![AllowancePeriodLimit::new(
            Some("1".into()),
            None,
            AllowancePeriod::anchored(1, AllowancePeriodUnit::Day, "2026-01-01T00:00:00Z").unwrap(),
        )
        .unwrap()])
        .build()
        .unwrap();
    let now = time("2026-01-02T00:00:00Z");
    let request = request("1", "btc");
    let usage = [usage("1", now)];
    let input = AllowanceEvaluationInput {
        terms: &terms,
        request: &request,
        trusted_time: time("2026-01-01T23:59:59Z"),
        watermark: now,
        usage: &usage,
    };
    assert_eq!(
        evaluate_allowance(&input),
        Err(AllowanceEvaluationBlock::ClockRollback)
    );
    assert!(evaluate(&terms, "1", time("2026-01-03T00:00:00Z"), &usage).is_ok());
}

mod periods;
