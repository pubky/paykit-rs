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
    if size_with_outcomes(&ledger)? > limit * 3 / 4 {
        if let Some(smaller) = compact(terms, &ledger, at)? {
            ledger = smaller;
        }
    }
    if size_with_outcomes(&ledger)? > limit {
        return Err(crate::Refusal::NotNow(
            "the Allowance ledger has no room for the payment and its outcome".into(),
        )
        .into());
    }
    Ok(ledger)
}
