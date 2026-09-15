use super::*;
use chrono::TimeDelta;

#[test]
fn test_anchored_months_clamp_from_original_anchor() {
    let period =
        AllowancePeriod::anchored(1, AllowancePeriodUnit::Month, "2026-01-31T12:30:00Z").unwrap();
    for (now, start, end) in [
        (
            "2026-02-28T12:30:00Z",
            "2026-02-28T12:30:00Z",
            "2026-03-31T12:30:00Z",
        ),
        (
            "2026-03-30T12:30:00Z",
            "2026-02-28T12:30:00Z",
            "2026-03-31T12:30:00Z",
        ),
        (
            "2025-12-01T00:00:00Z",
            "2025-11-30T12:30:00Z",
            "2025-12-31T12:30:00Z",
        ),
    ] {
        let window = allowance_period_window(&period, time(now)).unwrap();
        assert_eq!(
            (window.starts_at(), window.ends_at()),
            (time(start), time(end))
        );
        assert!(window.contains(time(start)));
        assert!(!window.contains(time(end)));
    }
}

#[test]
fn test_anchored_years_restore_leap_day_and_support_preanchor_time() {
    let period =
        AllowancePeriod::anchored(1, AllowancePeriodUnit::Year, "2024-02-29T00:00:00Z").unwrap();
    for (now, start, end) in [
        (
            "2025-02-28T00:00:00Z",
            "2025-02-28T00:00:00Z",
            "2026-02-28T00:00:00Z",
        ),
        (
            "2028-02-29T00:00:00Z",
            "2028-02-29T00:00:00Z",
            "2029-02-28T00:00:00Z",
        ),
        (
            "2023-02-28T00:00:00Z",
            "2023-02-28T00:00:00Z",
            "2024-02-29T00:00:00Z",
        ),
    ] {
        let window = allowance_period_window(&period, time(now)).unwrap();
        assert_eq!(
            (window.starts_at(), window.ends_at()),
            (time(start), time(end))
        );
    }
}

#[test]
fn test_fixed_periods_preserve_subseconds_and_negative_indices() {
    let anchor = "2026-01-01T00:00:00.500Z";
    for (unit, seconds) in [
        (AllowancePeriodUnit::Minute, 60),
        (AllowancePeriodUnit::Hour, 3600),
        (AllowancePeriodUnit::Day, 86400),
        (AllowancePeriodUnit::Week, 604800),
    ] {
        let period = AllowancePeriod::anchored(2, unit, anchor).unwrap();
        let window = allowance_period_window(&period, time("2026-01-01T00:00:00.499Z")).unwrap();
        assert_eq!(window.ends_at(), time(anchor));
        assert_eq!(
            window.starts_at(),
            time(anchor) - TimeDelta::seconds(2 * seconds)
        );
    }
}

#[test]
fn test_unrepresentable_period_arithmetic_fails_closed() {
    for period in [
        AllowancePeriod::rolling(u64::MAX, AllowancePeriodUnit::Week).unwrap(),
        AllowancePeriod::anchored(u64::MAX, AllowancePeriodUnit::Year, "2026-01-01T00:00:00Z")
            .unwrap(),
        AllowancePeriod::anchored(u64::MAX, AllowancePeriodUnit::Month, "2026-01-01T00:00:00Z")
            .unwrap(),
    ] {
        assert_eq!(
            allowance_period_window(&period, time("2026-01-01T00:00:00Z")),
            Err(AllowanceEvaluationBlock::ArithmeticOverflow)
        );
    }
    let period = AllowancePeriod::rolling(1, AllowancePeriodUnit::Day).unwrap();
    assert_eq!(
        allowance_period_window(&period, DateTime::<Utc>::MIN_UTC),
        Err(AllowanceEvaluationBlock::ArithmeticOverflow)
    );
    let period =
        AllowancePeriod::anchored(1, AllowancePeriodUnit::Month, "2026-01-01T00:00:00Z").unwrap();
    assert_eq!(
        allowance_period_window(&period, DateTime::<Utc>::MAX_UTC),
        Err(AllowanceEvaluationBlock::ArithmeticOverflow)
    );
}

#[test]
fn test_fixed_period_before_unix_epoch_preserves_fractional_boundary() {
    let period =
        AllowancePeriod::anchored(1, AllowancePeriodUnit::Minute, "1970-01-01T00:00:00.500Z")
            .unwrap();
    let window = allowance_period_window(&period, time("1969-12-31T23:59:59.999Z")).unwrap();
    assert_eq!(window.starts_at(), time("1969-12-31T23:59:00.500Z"));
    assert_eq!(window.ends_at(), time("1970-01-01T00:00:00.500Z"));
}

#[test]
fn test_leap_second_input_fails_closed_instead_of_normalizing() {
    let leap = time("2016-12-31T23:59:60Z");
    let period = AllowancePeriod::rolling(1, AllowancePeriodUnit::Minute).unwrap();
    assert_eq!(
        allowance_period_window(&period, leap),
        Err(AllowanceEvaluationBlock::ArithmeticOverflow)
    );
    assert!(AllowanceUsageEntry::new(PaymentAmount::new("1", "btc").unwrap(), leap).is_err());
}
