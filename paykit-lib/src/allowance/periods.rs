//! UTC period boundaries calculated from the original anchor.

use chrono::{DateTime, Datelike, NaiveDate, TimeDelta, Utc};

use super::{AllowanceEvaluationBlock, AllowancePeriod, AllowancePeriodKind, AllowancePeriodUnit};
use crate::validation::parse_utc_timestamp;

type MathResult<T> = std::result::Result<T, AllowanceEvaluationBlock>;
const OVERFLOW: AllowanceEvaluationBlock = AllowanceEvaluationBlock::ArithmeticOverflow;
const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// One exact UTC usage window: anchored `[start, end)` or rolling `(start, end]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowancePeriodWindow {
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    kind: AllowancePeriodKind,
}

impl AllowancePeriodWindow {
    /// Start boundary; inclusive only for anchored windows.
    pub fn starts_at(&self) -> DateTime<Utc> {
        self.starts_at
    }

    /// End boundary; inclusive only for rolling windows.
    pub fn ends_at(&self) -> DateTime<Utc> {
        self.ends_at
    }

    /// Whether the original admission time contributes to this window.
    pub fn contains(&self, admitted_at: DateTime<Utc>) -> bool {
        match self.kind {
            AllowancePeriodKind::Anchored => {
                self.starts_at <= admitted_at && admitted_at < self.ends_at
            }
            AllowancePeriodKind::Rolling => {
                self.starts_at < admitted_at && admitted_at <= self.ends_at
            }
        }
    }
}

/// Calculate a usage period at an explicit trusted time without reading a clock.
///
/// Calendar boundaries always derive from the original anchor, including before
/// that anchor. Arithmetic outside the representable UTC range fails closed.
/// Leap-second instants cannot be represented by this fixed-second math and
/// return `ArithmeticOverflow` rather than being silently normalized.
pub fn allowance_period_window(
    period: &AllowancePeriod,
    trusted_time: DateTime<Utc>,
) -> MathResult<AllowancePeriodWindow> {
    timestamp_nanos(trusted_time)?;
    let (starts_at, ends_at) = match period.kind() {
        AllowancePeriodKind::Rolling => {
            let duration = TimeDelta::try_seconds(fixed_seconds(period)?).ok_or(OVERFLOW)?;
            (
                trusted_time.checked_sub_signed(duration).ok_or(OVERFLOW)?,
                trusted_time,
            )
        }
        AllowancePeriodKind::Anchored => anchored_window(period, trusted_time)?,
    };
    Ok(AllowancePeriodWindow {
        starts_at,
        ends_at,
        kind: period.kind(),
    })
}

fn anchored_window(
    period: &AllowancePeriod,
    time: DateTime<Utc>,
) -> MathResult<(DateTime<Utc>, DateTime<Utc>)> {
    let anchor = parse_utc_timestamp(period.anchor().ok_or(OVERFLOW)?, "Allowance anchor")
        .map_err(|_| OVERFLOW)?
        .with_timezone(&Utc);
    timestamp_nanos(anchor)?;
    match period.unit() {
        AllowancePeriodUnit::Month | AllowancePeriodUnit::Year => {
            calendar_window(period, anchor, time)
        }
        _ => fixed_window(period, anchor, time),
    }
}

fn fixed_seconds(period: &AllowancePeriod) -> MathResult<i64> {
    let factor = match period.unit() {
        AllowancePeriodUnit::Minute => 60,
        AllowancePeriodUnit::Hour => 3600,
        AllowancePeriodUnit::Day => 86400,
        AllowancePeriodUnit::Week => 604800,
        _ => return Err(OVERFLOW),
    };
    i64::try_from(period.every().checked_mul(factor).ok_or(OVERFLOW)?).map_err(|_| OVERFLOW)
}

fn fixed_window(
    period: &AllowancePeriod,
    anchor: DateTime<Utc>,
    time: DateTime<Utc>,
) -> MathResult<(DateTime<Utc>, DateTime<Utc>)> {
    let length = i128::from(fixed_seconds(period)?)
        .checked_mul(NANOS_PER_SECOND)
        .ok_or(OVERFLOW)?;
    let anchor = timestamp_nanos(anchor)?;
    let k = timestamp_nanos(time)?
        .checked_sub(anchor)
        .ok_or(OVERFLOW)?
        .div_euclid(length);
    let start = anchor
        .checked_add(k.checked_mul(length).ok_or(OVERFLOW)?)
        .ok_or(OVERFLOW)?;
    Ok((
        from_nanos(start)?,
        from_nanos(start.checked_add(length).ok_or(OVERFLOW)?)?,
    ))
}

fn timestamp_nanos(time: DateTime<Utc>) -> MathResult<i128> {
    if time.timestamp_subsec_nanos() >= 1_000_000_000 {
        return Err(OVERFLOW);
    }
    i128::from(time.timestamp())
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|seconds| seconds.checked_add(i128::from(time.timestamp_subsec_nanos())))
        .ok_or(OVERFLOW)
}

fn from_nanos(value: i128) -> MathResult<DateTime<Utc>> {
    let seconds = i64::try_from(value.div_euclid(NANOS_PER_SECOND)).map_err(|_| OVERFLOW)?;
    let nanos = u32::try_from(value.rem_euclid(NANOS_PER_SECOND)).map_err(|_| OVERFLOW)?;
    DateTime::from_timestamp(seconds, nanos).ok_or(OVERFLOW)
}

fn calendar_window(
    period: &AllowancePeriod,
    anchor: DateTime<Utc>,
    time: DateTime<Utc>,
) -> MathResult<(DateTime<Utc>, DateTime<Utc>)> {
    let months = period
        .every()
        .checked_mul(if period.unit() == AllowancePeriodUnit::Year {
            12
        } else {
            1
        })
        .ok_or(OVERFLOW)?;
    let step = i64::try_from(months).map_err(|_| OVERFLOW)?;
    let mut k = (month_index(time) - month_index(anchor)).div_euclid(step);
    let mut start = calendar_boundary(anchor, k, step)?;
    if start > time {
        k = k.checked_sub(1).ok_or(OVERFLOW)?;
        start = calendar_boundary(anchor, k, step)?;
    }
    Ok((
        start,
        calendar_boundary(anchor, k.checked_add(1).ok_or(OVERFLOW)?, step)?,
    ))
}

fn month_index(time: DateTime<Utc>) -> i64 {
    i64::from(time.year()) * 12 + i64::from(time.month0())
}

fn calendar_boundary(anchor: DateTime<Utc>, k: i64, step: i64) -> MathResult<DateTime<Utc>> {
    let index = month_index(anchor)
        .checked_add(k.checked_mul(step).ok_or(OVERFLOW)?)
        .ok_or(OVERFLOW)?;
    let year = i32::try_from(index.div_euclid(12)).map_err(|_| OVERFLOW)?;
    let month = u32::try_from(index.rem_euclid(12) + 1).map_err(|_| OVERFLOW)?;
    let day = anchor.day().min(days_in_month(year, month));
    let date = NaiveDate::from_ymd_opt(year, month, day).ok_or(OVERFLOW)?;
    Ok(date.and_time(anchor.time()).and_utc())
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}
