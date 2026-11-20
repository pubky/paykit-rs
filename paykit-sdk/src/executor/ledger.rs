//! Rebuild an executor reservation with the complete compacted ledger (Paykit wire 5.5).

use crate::Result;
use paykit_lib::allowances::{compact, AllowanceTerms, EntryOutcome, Ledger, LedgerEntry};

/// Reserve room for every unresolved entry to take the larger final outcome.
fn size_with_outcomes(ledger: &Ledger) -> Result<usize> {
    let mut resolved = ledger.clone();
    for entry in &mut resolved.entries {
        entry.outcome.get_or_insert(EntryOutcome::Failed);
    }
    Ok(resolved.to_json()?.len())
}

/// The complete write for an opening, including any entries moved to totals (Paykit wire 5.5).
pub(super) fn reserved_ledger(
    terms: &AllowanceTerms,
    mut ledger: Ledger,
    entry: LedgerEntry,
) -> Result<Ledger> {
    let at = entry.at;
    ledger.entries.push(entry);
    let limit = 256 * 1024 - 4;
    let mut size = size_with_outcomes(&ledger)?;
    if size > limit * 3 / 4 {
        if let Some(smaller) = compact(terms, &ledger, at)? {
            size = size_with_outcomes(&smaller)?;
            ledger = smaller;
        }
    }
    if size > limit {
        return Err(crate::Refusal::NotNow(
            "the Allowance ledger has no room for the payment and its outcome".into(),
        )
        .into());
    }
    Ok(ledger)
}

#[cfg(test)]
mod tests {
    use super::*;
    use paykit_lib::{AllowanceId, Asset, AttemptId, Decimal};
    use serde_json::{json, Value};

    // Retain an unknown member so the reservation with all outcomes can reach an exact byte.
    fn reservation(
        size: usize,
        previous: Option<EntryOutcome>,
    ) -> (AllowanceTerms, Ledger, LedgerEntry) {
        let terms = AllowanceTerms::from_json(&json!({
            "asset":"btc","lifetime_amount_limit":"10","period_limits":[],
            "per_payment_amount":null,"active_from":null,"expires_at":null,
            "allowed_payment_endpoint_identifiers":null
        }))
        .unwrap();
        let entry = LedgerEntry {
            obligation: [1; 32],
            attempt_id: AttemptId::from_bytes([1; 16]),
            value: Decimal::new("1").unwrap(),
            asset: Asset::new("btc").unwrap(),
            at: 1,
            outcome: previous,
        };
        let next = LedgerEntry {
            obligation: [2; 32],
            attempt_id: AttemptId::from_bytes([2; 16]),
            at: 120_000,
            outcome: None,
            ..entry.clone()
        };
        let mut ledger = Ledger::empty(
            [3; 32],
            AllowanceId::new("4b3fb324-091b-47da-a7ad-46b3a07d0409").unwrap(),
            [4; 32],
        );
        ledger.entries.push(entry);
        let mut value: Value = serde_json::from_slice(&ledger.to_json().unwrap()).unwrap();
        value["padding"] = json!("");
        let mut full = Ledger::from_map(value.as_object().unwrap()).unwrap();
        full.entries.push(next.clone());
        for entry in &mut full.entries {
            entry.outcome.get_or_insert(EntryOutcome::Failed);
        }
        let padding = size - full.to_json().unwrap().len();
        value["padding"] = Value::String("x".repeat(padding));
        (
            terms,
            Ledger::from_map(value.as_object().unwrap()).unwrap(),
            next,
        )
    }

    #[test]
    fn test_reservation_compacts_only_above_three_quarters_and_checks_the_new_size() {
        let limit = 256 * 1024 - 4;
        for size in [limit * 3 / 4, limit * 3 / 4 + 1, limit + 1] {
            let (terms, ledger, next) = reservation(size, Some(EntryOutcome::Done));
            let mut expected = ledger.clone();
            expected.entries.push(next.clone());
            if size > limit * 3 / 4 {
                expected = compact(&terms, &expected, next.at).unwrap().unwrap();
            }
            let applied = reserved_ledger(&terms, ledger, next).unwrap();
            assert_eq!(applied.to_json().unwrap(), expected.to_json().unwrap());
        }
    }

    #[test]
    fn test_reservation_keeps_room_for_each_unresolved_outcome_at_the_limit() {
        let limit = 256 * 1024 - 4;
        for size in [limit, limit + 1] {
            let (terms, ledger, next) = reservation(size, None);
            let mut expected = ledger.clone();
            expected.entries.push(next.clone());
            assert!(expected.to_json().unwrap().len() < limit);
            let result = reserved_ledger(&terms, ledger, next);
            if size == limit {
                assert_eq!(
                    result.unwrap().to_json().unwrap(),
                    expected.to_json().unwrap()
                );
            } else {
                assert!(matches!(
                    result,
                    Err(crate::PaykitError::Refused(crate::Refusal::NotNow(_)))
                ));
            }
        }
    }
}
