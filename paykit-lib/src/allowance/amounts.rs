//! Exact nonnegative decimal arithmetic without an asset precision policy.

use std::cmp::Ordering;

use crate::{validation::validate_decimal_text, Result};

/// Compare valid Payment Amount decimal spellings numerically, without rounding.
///
/// ```
/// use paykit_lib::compare_decimal_amounts;
/// assert!(compare_decimal_amounts(".50", "0.500")?.is_eq());
/// # Ok::<(), paykit_lib::PaykitError>(())
/// ```
pub fn compare_decimal_amounts(left: &str, right: &str) -> Result<Ordering> {
    validate_decimal_text(left, "left Payment Amount")?;
    validate_decimal_text(right, "right Payment Amount")?;
    Ok(compare_decimals(left, right))
}

/// Add valid Payment Amount decimal spellings exactly, without a precision limit.
///
/// The result uses no redundant leading or fractional trailing zeros. Inputs are
/// not modified; retain their original spellings when comparing wire messages.
///
/// ```
/// use paykit_lib::add_decimal_amounts;
/// assert_eq!(add_decimal_amounts(".1", "0.20")?, "0.3");
/// # Ok::<(), paykit_lib::PaykitError>(())
/// ```
pub fn add_decimal_amounts(left: &str, right: &str) -> Result<String> {
    validate_decimal_text(left, "left Payment Amount")?;
    validate_decimal_text(right, "right Payment Amount")?;
    Ok(add_decimals(left, right))
}

pub(super) fn compare_decimals(left: &str, right: &str) -> Ordering {
    let (left_integer, left_fraction) = decimal_parts(left);
    let (right_integer, right_fraction) = decimal_parts(right);
    left_integer
        .len()
        .cmp(&right_integer.len())
        .then_with(|| left_integer.cmp(right_integer))
        .then_with(|| {
            let width = left_fraction.len().max(right_fraction.len());
            left_fraction
                .bytes()
                .chain(std::iter::repeat(b'0'))
                .take(width)
                .cmp(
                    right_fraction
                        .bytes()
                        .chain(std::iter::repeat(b'0'))
                        .take(width),
                )
        })
}

pub(super) fn add_decimals(left: &str, right: &str) -> String {
    let (left_integer, left_fraction) = decimal_parts(left);
    let (right_integer, right_fraction) = decimal_parts(right);
    let scale = left_fraction.len().max(right_fraction.len());
    let left_digits = scaled_digits(left_integer, left_fraction, scale);
    let right_digits = scaled_digits(right_integer, right_fraction, scale);
    let width = left_digits.len().max(right_digits.len());
    let mut sum = Vec::with_capacity(width + 1);
    let mut carry = 0;
    for index in 0..width {
        let value = left_digits.get(index).copied().unwrap_or(0)
            + right_digits.get(index).copied().unwrap_or(0)
            + carry;
        sum.push(b'0' + value % 10);
        carry = value / 10;
    }
    if carry != 0 {
        sum.push(b'0' + carry);
    }
    sum.reverse();
    let mut result = sum.into_iter().map(char::from).collect::<String>();
    if scale != 0 {
        result.insert(result.len() - scale, '.');
        result = result
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned();
    }
    result
}

fn scaled_digits(integer: &str, fraction: &str, scale: usize) -> Vec<u8> {
    std::iter::repeat_n(0, scale - fraction.len())
        .chain(fraction.bytes().rev().map(|digit| digit - b'0'))
        .chain(integer.bytes().rev().map(|digit| digit - b'0'))
        .collect()
}

fn decimal_parts(value: &str) -> (&str, &str) {
    let (integer, fraction) = value.split_once('.').unwrap_or((value, ""));
    let integer = integer.trim_start_matches('0');
    (
        if integer.is_empty() { "0" } else { integer },
        fraction.trim_end_matches('0'),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decimal_spellings_compare_and_add_exactly() {
        for (left, right, sum) in [
            (".5", "00.500", "1"),
            ("10.", "0.000", "10"),
            (".009", ".001", "0.01"),
            ("000", ".0", "0"),
        ] {
            assert_eq!(add_decimal_amounts(left, right).unwrap(), sum);
        }
        assert_eq!(
            compare_decimal_amounts("01.000", "1").unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            compare_decimal_amounts(".509", ".51").unwrap(),
            Ordering::Less
        );
        for invalid in ["", ".", "-1", "+1", "1e2", "1,000"] {
            assert!(add_decimal_amounts(invalid, "0").is_err());
            assert!(compare_decimal_amounts("0", invalid).is_err());
        }
    }

    #[test]
    fn test_decimal_arithmetic_preserves_large_precision() {
        let nines = "9".repeat(1200);
        assert_eq!(
            add_decimal_amounts(&nines, "1").unwrap(),
            format!("1{}", "0".repeat(1200))
        );
        let tiny = format!("0.{}1", "0".repeat(1200));
        assert_eq!(
            add_decimal_amounts("1", &tiny).unwrap(),
            format!("1.{}1", "0".repeat(1200))
        );
        assert!(compare_decimal_amounts(&tiny, "0").unwrap().is_gt());
    }

    proptest::proptest! {
        #[test]
        fn test_decimal_addition_matches_integer_arithmetic(left: u64, right: u64) {
            proptest::prop_assert_eq!(add_decimal_amounts(&left.to_string(), &right.to_string()).unwrap(),
                (u128::from(left) + u128::from(right)).to_string());
        }

        #[test]
        fn test_decimal_addition_matches_different_integer_scales(left: u64, right: u64) {
            let left_text = format!("{}.{:02}", left / 100, left % 100);
            let right_text = format!("{}.{:03}", right / 1000, right % 1000);
            let expected = u128::from(left) * 10 + u128::from(right);
            let expected_text = format!("{}.{:03}", expected / 1000, expected % 1000);
            proptest::prop_assert_eq!(add_decimal_amounts(&left_text, &right_text).unwrap(),
                expected_text.trim_end_matches('0').trim_end_matches('.'));
        }
    }
}
