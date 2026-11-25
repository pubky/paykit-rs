//! Same-executor accounting after a device restore (Paykit spec 8.3).

mod approval;
mod checkpoint;
mod compaction;
mod evidence;
mod grant;
mod ledger;
#[cfg(feature = "test-support")]
pub use ledger::with_early_ledger_compaction;
mod outcomes;
mod preparation;
pub(crate) use evidence::OwnWrite;
pub(crate) use preparation::LedgerProposal;
mod restore;
mod spend;
pub(crate) use checkpoint::ExecutorHistory;
pub(crate) use compaction::LedgerCompaction;
pub(crate) use spend::{standing, AllowanceReservation, JournalAllowance, Standing};

use std::collections::{HashMap, HashSet};

use paykit_lib::allowances::{usage, AllowanceTerms, AttemptEvidence, ClaimState, Ledger, Usage};
use paykit_lib::AttemptId;

use crate::rail::{AllowanceHandoff, Rail, RailOutcome};

/// The proposal must belong to the Allower's surviving history as of the grant.
pub(crate) fn in_grant_history(
    proposed: u64,
    grant_generation: u64,
    recovery_from: Option<u64>,
) -> bool {
    proposed > recovery_from.unwrap_or(0) && proposed <= grant_generation
}

/// Claim state for a verified reservation. A settled tombstone for an already accepted
/// payment also needs the dated payer proof and the independently matched transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReservationClaim {
    obligation: [u8; 32],
    state: ClaimState,
}

/// A complete wallet inventory can still contain an unclassified payment. Its
/// original attempt must be attributed before a missing service entry is an alarm.
struct Accounting {
    counted: Usage,
    unclassified: Vec<AllowanceHandoff>,
    /// The evidence the ledger write takes, from the same verdicts as `counted`.
    writes: HashMap<AttemptId, AttemptEvidence>,
}

async fn wallet_inventory(rail: &dyn Rail, payer: [u8; 32]) -> Option<Vec<AllowanceHandoff>> {
    let declaration = rail.declaration();
    if !declaration.history || !declaration.history_survives_restore {
        return None;
    }
    rail.allowance_history(payer).await.ok()?
}

/// What the payment method's history shows of an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MethodShows {
    /// The history doesn't list it.
    Nothing,
    /// Failed.
    Failed,
    /// Still pending.
    Pending,
    /// Paid.
    Paid,
}

impl From<&RailOutcome> for MethodShows {
    fn from(outcome: &RailOutcome) -> Self {
        match outcome {
            RailOutcome::Failed => Self::Failed,
            RailOutcome::Pending => Self::Pending,
            RailOutcome::Paid(_) => Self::Paid,
        }
    }
}

/// What this app holds of an attempt's claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimShows {
    /// The claim, or the attempt in it, is gone, or nothing verifies what it shows.
    Gone,
    /// The freshest state this app verifies against the attempt's signed opening and
    /// outcomes; a later signed `done` wins over a read that lags.
    Verified(ClaimState),
    /// A signed `done` this app holds, without the opening it follows.
    SignedDone,
    /// `done`, which only the read shows: nothing signed verifies it.
    ShownDone,
}

/// Everything this app holds of one attempt the ledger lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seen {
    claim: ClaimShows,
    /// A payer proof of the attempt in a dated write of the current payer record.
    dated_proof: bool,
    /// A payer proof of the attempt that can't be dated now: only this app's copy of the
    /// record holds it, or its write isn't held.
    undated_proof: bool,
    /// This installation saw the payment made before, from any source, and keeps that
    /// sighting ([`Paykit::paid_seen`]).
    sighted: bool,
    method: MethodShows,
}

/// One verdict on an attempt, which both the usage count and the ledger write take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Verdict {
    /// The evidence usage counts by.
    counts: AttemptEvidence,
    /// The evidence an outcome copied into the ledger may rest on.
    writes: AttemptEvidence,
}

/// The verdict on an attempt (Paykit spec 8.3, `failed` releases: "an attempt whose outcome
/// is `failed` releases its reservation"; "a claim or ledger entry that later shows `done`,
/// or a payer record holding a proof of this attempt naming this Allowance, counts it as
/// spent again"). Anything this app holds that says the money moved counts the attempt
/// spent: a proof, dated or not, a claim showing `done`, signed or only read, or the
/// method's history showing it paid; counting a payment that may have been made is the
/// safe answer. Only what another installation can check is written: a dated proof or a
/// signed `done`. A payment counted spent on anything else writes nothing, so no ledger
/// write ever records `failed` for an attempt this app counts paid. Without such evidence,
/// the failure the wallet recorded releases, as the spec says the wallet decides.
fn attempt_verdict(seen: Seen) -> Verdict {
    let claim = match seen.claim {
        ClaimShows::Verified(state) => Some(state),
        ClaimShows::SignedDone => Some(ClaimState::Done),
        ClaimShows::ShownDone | ClaimShows::Gone => None,
    };
    let provable = seen.dated_proof || claim == Some(ClaimState::Done);
    let method_paid = match seen.method {
        MethodShows::Paid => true,
        MethodShows::Nothing | MethodShows::Failed | MethodShows::Pending => false,
    };
    let paid = provable
        || seen.undated_proof
        || seen.sighted
        || seen.claim == ClaimShows::ShownDone
        || method_paid;
    let counts = AttemptEvidence { claim, paid };
    let writes = match (paid, provable) {
        // Counted paid on evidence another installation can't check: nothing is written.
        (true, false) => AttemptEvidence::default(),
        (true, true) | (false, _) => AttemptEvidence {
            claim,
            paid: seen.dated_proof,
        },
    };
    Verdict { counts, writes }
}

// Each input is a separate piece of evidence the count must see, named so a caller can't
// leave one out unnoticed; bundling them would hide which one a caller lacks.
#[allow(clippy::too_many_arguments)]
fn reconciled_usage(
    inventory: Vec<AllowanceHandoff>,
    terms: &AllowanceTerms,
    ledger: &Ledger,
    claims: &HashMap<AttemptId, ReservationClaim>,
    unverified: &HashMap<AttemptId, ClaimShows>,
    retired: &mut HashMap<AttemptId, compaction::Retired>,
    dated: HashMap<AttemptId, AttemptEvidence>,
    undated: &HashSet<AttemptId>,
    sighted: &HashSet<AttemptId>,
    at: u64,
) -> Option<Accounting> {
    let mut unclassified = Vec::new();
    let mut seen = HashSet::new();
    let mut methods = HashMap::new();
    for entry in &ledger.entries {
        if claims
            .get(&entry.attempt_id)
            .is_some_and(|claim| claim.obligation != entry.obligation)
        {
            return None;
        }
    }
    let proved = |id: &AttemptId| {
        dated.get(id).is_some_and(|fact| fact.paid)
            || undated.contains(id)
            || sighted.contains(id)
            || matches!(
                unverified.get(id),
                Some(ClaimShows::SignedDone | ClaimShows::ShownDone)
            )
    };
    for handoff in inventory {
        let id = handoff.attempt_id;
        if !seen.insert(id) {
            return None;
        }
        let entry = ledger.entries.iter().find(|e| e.attempt_id == id);
        let failed = ledger.failed.iter().find(|f| f.attempt_id == id);
        let member = entry
            .map(|e| (&e.obligation, &e.asset, &e.value))
            .or_else(|| failed.map(|f| (&f.obligation, &terms.asset, &f.value)));
        let Some((obligation, asset, value)) = member else {
            if let Some(entry) = retired.get(&id).filter(|entry| {
                matches!(&handoff.outcome, RailOutcome::Failed)
                    || matches!(
                        (entry, &handoff.outcome),
                        (
                            compaction::Retired::PreviouslyFailed(_),
                            RailOutcome::Paid(_)
                        )
                    )
            }) {
                // The authenticated ledger transition already counted this reservation
                // as paid. The same transfer can still show failed in the method's
                // inventory or subsequently agree with the copied claim.
                let (asset, value) = match entry {
                    compaction::Retired::Paid(e) => (&e.asset, &e.value),
                    compaction::Retired::PreviouslyFailed(e) => (&terms.asset, &e.value),
                };
                if &handoff.amount.asset != asset || &handoff.amount.value != value {
                    return None;
                }
                retired.remove(&id);
                continue;
            }
            unclassified.push(handoff);
            continue;
        };
        let claim = claims.get(&id);
        // A proof or a `done` accounts for the payment and counts it spent; an entry's own
        // recorded `failed` accounts for it as the failure the wallet decided, also once its
        // claim is gone (Paykit spec 8.3, `failed` releases).
        let payer_proved = proved(&id);
        let recorded_failure = entry.is_some_and(|entry| {
            entry.outcome == Some(paykit_lib::allowances::EntryOutcome::Failed)
        });
        if claim.is_some_and(|claim| *obligation != claim.obligation)
            || (claim.is_none() && !payer_proved && !recorded_failure)
            || asset != &handoff.amount.asset
            || value != &handoff.amount.value
        {
            return None;
        }
        methods.insert(id, MethodShows::from(&handoff.outcome));
        match failed {
            None => {}
            // v0.2's check on a compacted failure, kept: the method's history showing it
            // pending, or failed with no claim that shows the failure, holds the ledger unless
            // a proof or a `done` shows it paid. The spec's rule for a recorded failure doesn't
            // ask for this (Paykit spec 8.3, `failed` releases); whether to ask it of an
            // entry too is the spec's to say.
            Some(_) => {
                let settled =
                    payer_proved || claim.is_some_and(|claim| claim.state == ClaimState::Done);
                let failed_claim =
                    claim.is_some_and(|claim| matches!(claim.state, ClaimState::Failed { .. }));
                match &handoff.outcome {
                    RailOutcome::Paid(_) => {}
                    RailOutcome::Failed if settled || failed_claim => {}
                    RailOutcome::Pending if settled => {}
                    RailOutcome::Failed | RailOutcome::Pending => return None,
                }
            }
        }
    }
    // One verdict per attempt the ledger lists, for the count and the write alike.
    let mut counts = HashMap::new();
    let mut writes = HashMap::new();
    for id in ledger
        .entries
        .iter()
        .map(|entry| entry.attempt_id)
        .chain(ledger.failed.iter().map(|item| item.attempt_id))
    {
        let claim = match (claims.get(&id), unverified.get(&id)) {
            (Some(claim), _) => ClaimShows::Verified(claim.state),
            (None, Some(shown)) => *shown,
            (None, None) => ClaimShows::Gone,
        };
        let verdict = attempt_verdict(Seen {
            claim,
            dated_proof: dated.get(&id).is_some_and(|fact| fact.paid),
            undated_proof: undated.contains(&id),
            sighted: sighted.contains(&id),
            method: methods.get(&id).copied().unwrap_or(MethodShows::Nothing),
        });
        counts.insert(id, verdict.counts);
        writes.insert(id, verdict.writes);
    }
    Some(Accounting {
        counted: usage(terms, ledger, &counts, at).ok()?,
        unclassified,
        writes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rail::FakeRail;
    use paykit_lib::allowances::{check_limits, LedgerEntry, LimitKind};
    use paykit_lib::{
        AllowanceId, Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier,
        PaymentEndpointPayload,
    };
    use pubky_dispatch_client::clock::TestClock;
    use std::sync::Arc;

    #[test]
    fn test_grant_uses_the_proposals_authenticated_generation_interval() {
        assert!(in_grant_history(2, 2, Some(1)));
        assert!(in_grant_history(1, 2, None));
        assert!(!in_grant_history(3, 2, None));
        assert!(!in_grant_history(1, 2, Some(1)));
    }

    async fn read_usage(
        rail: &dyn Rail,
        payer: [u8; 32],
        terms: &AllowanceTerms,
        ledger: &Ledger,
        claims: &HashMap<AttemptId, ReservationClaim>,
        at: u64,
    ) -> Option<Accounting> {
        reconciled_usage(
            wallet_inventory(rail, payer).await?,
            terms,
            ledger,
            claims,
            &HashMap::new(),
            &mut HashMap::new(),
            HashMap::new(),
            &HashSet::new(),
            &HashSet::new(),
            at,
        )
    }
    #[test]
    fn test_the_verdict_counts_and_writes_as_the_table_says() {
        use paykit_lib::allowances::{record_outcomes, CompactedFailed, EntryOutcome, LedgerEntry};
        use Claim::*;
        use Held::*;
        use Method::*;
        use Proof::*;
        use Written::*;

        #[derive(Clone, Copy, Debug)]
        enum Held {
            Recorded,
            Unset,
            Compacted,
        }
        #[derive(Clone, Copy, Debug)]
        enum Method {
            NotListed,
            HistFailed,
            HistPending,
            HistPaid,
        }
        #[derive(Clone, Copy, Debug)]
        enum Claim {
            Gone,
            VFailed,
            VDone,
            VOpen,
            VUnknown,
            SignedDone,
            ShownDone,
        }
        #[derive(Clone, Copy, Debug)]
        enum Proof {
            NoProof,
            Dated,
            Undated,
        }
        /// What the ledger holds for the attempt after the copy.
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Written {
            /// An entry with no outcome.
            Unset_,
            /// An entry, or a compacted item, holding `failed`.
            Failed_,
            /// An entry holding `done`, or a compacted item moved into the paid totals.
            Done_,
        }
        /// The whole ledger holds: no usage.
        const HOLD: Option<(&str, &str, Written)> = None;

        // (held, method, claim, proof, sighted) => (lifetime, reserved amount, written)
        let rows: &[(
            (Held, Method, Claim, Proof, bool),
            Option<(&str, &str, Written)>,
        )] = &[
            // An entry's own `failed`: it releases unless something says the money moved.
            (
                (Recorded, NotListed, Gone, NoProof, false),
                Some(("0", "0", Failed_)),
            ),
            (
                (Recorded, HistFailed, Gone, NoProof, false),
                Some(("0", "0", Failed_)),
            ),
            (
                (Recorded, HistPending, VFailed, NoProof, false),
                Some(("0", "0", Failed_)),
            ),
            (
                (Recorded, HistPaid, VFailed, NoProof, false),
                Some(("70", "0", Failed_)),
            ),
            (
                (Recorded, NotListed, VFailed, Dated, false),
                Some(("70", "0", Done_)),
            ),
            (
                (Recorded, NotListed, VFailed, Undated, false),
                Some(("70", "0", Failed_)),
            ),
            (
                (Recorded, NotListed, SignedDone, NoProof, false),
                Some(("70", "0", Done_)),
            ),
            (
                (Recorded, NotListed, ShownDone, NoProof, false),
                Some(("70", "0", Failed_)),
            ),
            (
                (Recorded, NotListed, VDone, NoProof, false),
                Some(("70", "0", Done_)),
            ),
            (
                (Recorded, HistFailed, VFailed, NoProof, true),
                Some(("70", "0", Failed_)),
            ),
            // No outcome yet: a verified failed claim writes `failed` only when nothing says paid.
            (
                (Unset, NotListed, VFailed, NoProof, false),
                Some(("0", "0", Failed_)),
            ),
            (
                (Unset, HistPending, VFailed, NoProof, false),
                Some(("0", "0", Failed_)),
            ),
            (
                (Unset, NotListed, VFailed, Undated, false),
                Some(("70", "0", Unset_)),
            ),
            (
                (Unset, HistPaid, VFailed, NoProof, false),
                Some(("70", "0", Unset_)),
            ),
            (
                (Unset, NotListed, VFailed, NoProof, true),
                Some(("70", "0", Unset_)),
            ),
            (
                (Unset, NotListed, VFailed, Dated, false),
                Some(("70", "0", Done_)),
            ),
            (
                (Unset, NotListed, VOpen, NoProof, false),
                Some(("70", "70", Unset_)),
            ),
            (
                (Unset, NotListed, VUnknown, NoProof, false),
                Some(("70", "70", Unset_)),
            ),
            (
                (Unset, NotListed, Gone, NoProof, false),
                Some(("70", "0", Unset_)),
            ),
            ((Unset, HistFailed, Gone, NoProof, false), HOLD),
            (
                (Unset, NotListed, SignedDone, NoProof, false),
                Some(("70", "0", Done_)),
            ),
            (
                (Unset, NotListed, ShownDone, NoProof, false),
                Some(("70", "0", Unset_)),
            ),
            // A compacted failure: v0.2's holds kept; a proven payment moves into the totals.
            (
                (Compacted, NotListed, Gone, NoProof, false),
                Some(("0", "0", Failed_)),
            ),
            (
                (Compacted, HistFailed, VFailed, NoProof, false),
                Some(("0", "0", Failed_)),
            ),
            ((Compacted, HistFailed, Gone, NoProof, false), HOLD),
            ((Compacted, HistPending, VFailed, NoProof, false), HOLD),
            ((Compacted, HistPaid, Gone, NoProof, false), HOLD),
            (
                (Compacted, NotListed, VFailed, Dated, false),
                Some(("70", "0", Done_)),
            ),
            (
                (Compacted, NotListed, SignedDone, NoProof, false),
                Some(("70", "0", Done_)),
            ),
            (
                (Compacted, NotListed, ShownDone, NoProof, false),
                Some(("70", "0", Failed_)),
            ),
            (
                (Compacted, NotListed, VFailed, Undated, false),
                Some(("70", "0", Failed_)),
            ),
            (
                (Compacted, NotListed, VFailed, NoProof, true),
                Some(("70", "0", Failed_)),
            ),
        ];

        let terms = AllowanceTerms::from_json(&serde_json::json!({
            "asset":"btc","lifetime_amount_limit":"100","period_limits":[],
            "per_payment_amount":null,"active_from":null,"expires_at":null,
            "allowed_payment_endpoint_identifiers":null
        }))
        .unwrap();
        let id = AttemptId::from_bytes([5; 16]);
        let obligation = [6; 32];
        let value = Decimal::new("70").unwrap();
        for &((held, method, claim, proof, sighted), expected) in rows {
            let case = format!("{held:?} {method:?} {claim:?} {proof:?} sighted {sighted}");
            let mut ledger = Ledger::empty(
                [7; 32],
                AllowanceId::new("4b3fb324-091b-47da-a7ad-46b3a07d0409").unwrap(),
                [8; 32],
            );
            let entry = |outcome| LedgerEntry {
                obligation,
                attempt_id: id,
                value: value.clone(),
                asset: terms.asset.clone(),
                at: 1_000,
                outcome,
            };
            match held {
                Recorded => ledger.entries.push(entry(Some(EntryOutcome::Failed))),
                Unset => ledger.entries.push(entry(None)),
                Compacted => {
                    ledger.failed.push(CompactedFailed {
                        obligation,
                        attempt_id: id,
                        value: value.clone(),
                    });
                    ledger.spent = Some(Decimal::new("0").unwrap());
                }
            }
            let verified = |state| HashMap::from([(id, ReservationClaim { obligation, state })]);
            let failed = ClaimState::Failed { by_owner: false };
            let (claims, unverified) = match claim {
                Gone => (HashMap::new(), HashMap::new()),
                VFailed => (verified(failed), HashMap::new()),
                VDone => (verified(ClaimState::Done), HashMap::new()),
                VOpen => (verified(ClaimState::Open), HashMap::new()),
                VUnknown => (verified(ClaimState::Unknown), HashMap::new()),
                SignedDone => (
                    HashMap::new(),
                    HashMap::from([(id, ClaimShows::SignedDone)]),
                ),
                ShownDone => (HashMap::new(), HashMap::from([(id, ClaimShows::ShownDone)])),
            };
            let outcome = match method {
                NotListed => None,
                HistFailed => Some(RailOutcome::Failed),
                HistPending => Some(RailOutcome::Pending),
                HistPaid => Some(RailOutcome::Paid(crate::rail::RailPaid {
                    proof: serde_json::Map::new(),
                    paid_at_ms: None,
                })),
            };
            let inventory = outcome
                .map(|outcome| AllowanceHandoff {
                    attempt_id: id,
                    amount: PaymentAmount {
                        asset: terms.asset.clone(),
                        value: value.clone(),
                    },
                    outcome,
                })
                .into_iter()
                .collect();
            let dated = match proof {
                Dated => HashMap::from([(
                    id,
                    AttemptEvidence {
                        paid: true,
                        ..Default::default()
                    },
                )]),
                NoProof | Undated => HashMap::new(),
            };
            let undated = match proof {
                Undated => HashSet::from([id]),
                NoProof | Dated => HashSet::new(),
            };
            let seen = match sighted {
                true => HashSet::from([id]),
                false => HashSet::new(),
            };
            let accounting = reconciled_usage(
                inventory,
                &terms,
                &ledger,
                &claims,
                &unverified,
                &mut HashMap::new(),
                dated,
                &undated,
                &seen,
                1_000_000,
            );
            let got = accounting.map(|accounting| {
                // The actual write: what the ledger holds after the outcomes are copied.
                let next = record_outcomes(&terms, &ledger, &accounting.writes)
                    .unwrap()
                    .unwrap_or_else(|| ledger.clone());
                let written = match (next.entries.first(), next.failed.is_empty()) {
                    (Some(entry), _) => match entry.outcome {
                        None => Unset_,
                        Some(EntryOutcome::Failed) => Failed_,
                        Some(EntryOutcome::Done) => Done_,
                    },
                    (None, false) => Failed_,
                    (None, true) => Done_,
                };
                let usage = accounting.counted;
                (
                    usage.lifetime().as_str().to_owned(),
                    usage.reserved_amount().as_str().to_owned(),
                    written,
                )
            });
            let expected = expected.map(|(lifetime, reserved, written)| {
                (lifetime.to_owned(), reserved.to_owned(), written)
            });
            assert_eq!(got, expected, "{case}");
        }
    }

    #[test]
    fn test_compacted_method_failure_releases_and_requires_its_original_claim() {
        use paykit_lib::allowances::CompactedFailed;
        let terms = AllowanceTerms::from_json(&serde_json::json!({
            "asset":"btc","lifetime_amount_limit":"100","period_limits":[],
            "per_payment_amount":null,"active_from":null,"expires_at":null,
            "allowed_payment_endpoint_identifiers":null
        }))
        .unwrap();
        let mut ledger = Ledger::empty(
            [7; 32],
            AllowanceId::new("4b3fb324-091b-47da-a7ad-46b3a07d0409").unwrap(),
            [8; 32],
        );
        let id = AttemptId::from_bytes([5; 16]);
        let value = Decimal::new("70").unwrap();
        ledger.failed.push(CompactedFailed {
            obligation: [6; 32],
            attempt_id: id,
            value: value.clone(),
        });
        ledger.spent = Some(Decimal::new("0").unwrap());
        let claims = HashMap::from([(
            id,
            ReservationClaim {
                obligation: [6; 32],
                state: ClaimState::Failed { by_owner: true },
            },
        )]);
        let handoff = AllowanceHandoff {
            attempt_id: id,
            amount: PaymentAmount {
                asset: terms.asset.clone(),
                value,
            },
            outcome: RailOutcome::Failed,
        };
        let read = |handoff, claims: &HashMap<_, _>| {
            reconciled_usage(
                vec![handoff],
                &terms,
                &ledger,
                claims,
                &HashMap::new(),
                &mut HashMap::new(),
                HashMap::new(),
                &HashSet::new(),
                &HashSet::new(),
                100_000_000,
            )
        };
        // The verified failure releases the 70, whichever own app wrote it (Paykit spec
        // 8.3, `failed` releases), and the method's history agrees.
        let accounting = read(handoff.clone(), &claims).unwrap();
        assert_eq!(accounting.counted.lifetime().as_str(), "0");
        assert!(accounting.unclassified.is_empty());
        let by_another = HashMap::from([(
            id,
            ReservationClaim {
                obligation: [6; 32],
                state: ClaimState::Failed { by_owner: false },
            },
        )]);
        let accounting = read(handoff.clone(), &by_another).unwrap();
        assert_eq!(accounting.counted.lifetime().as_str(), "0");
        assert!(accounting.unclassified.is_empty());
        assert!(read(handoff.clone(), &HashMap::new()).is_none());
        let mut different = handoff.clone();
        different.amount.value = Decimal::new("69").unwrap();
        assert!(read(different, &claims).is_none());
        // v0.2's check on a compacted failure, kept: the method's history still showing it
        // pending holds the ledger.
        let mut pending = handoff;
        pending.outcome = RailOutcome::Pending;
        assert!(read(pending, &claims).is_none());
    }

    #[tokio::test]
    async fn test_account_history_charges_the_open_seventy_after_restore() {
        // The wallet clock runs behind the Dispatch that dated the reservation at 1,001.
        let clock = Arc::new(TestClock::new(16));
        let rail = FakeRail::with_clock(clock.clone());
        let payer = [3; 32];
        rail.set_history_payer(payer);
        let terms = AllowanceTerms {
            asset: Asset::new("btc").unwrap(),
            per_payment_amount: None,
            period_limits: vec![],
            lifetime_amount_limit: Some(Decimal::new("100").unwrap()),
            active_from: None,
            expires_at: None,
            allowed_payment_endpoint_identifiers: None,
        };
        let mut ledger = Ledger::empty(
            [7; 32],
            AllowanceId::new("4b3fb324-091b-47da-a7ad-46b3a07d0409").unwrap(),
            [8; 32],
        );
        clock.advance_ms(1);
        let id = AttemptId::from_bytes([5; 16]);
        let value = Decimal::new("70").unwrap();
        let obligation = [6; 32];
        ledger.entries.push(LedgerEntry {
            obligation,
            attempt_id: id,
            value: value.clone(),
            asset: terms.asset.clone(),
            at: 1_001,
            outcome: None,
        });
        // Open, unknown, done and a missing claim keep the 70 counted; a failure releases it.
        for (state, lifetime, reserved, spent, released) in [
            (Some(ClaimState::Open), "70", 1, 0, 0),
            (Some(ClaimState::Unknown), "70", 1, 0, 0),
            (Some(ClaimState::Done), "70", 0, 1, 0),
            (Some(ClaimState::Failed { by_owner: true }), "0", 0, 0, 1),
            (Some(ClaimState::Failed { by_owner: false }), "0", 0, 0, 1),
            (None, "70", 0, 1, 0),
        ] {
            let claims = state
                .map(|state| (id, ReservationClaim { obligation, state }))
                .into_iter()
                .collect();
            let usage = reconciled_usage(
                Vec::new(),
                &terms,
                &ledger,
                &claims,
                &HashMap::new(),
                &mut HashMap::new(),
                HashMap::new(),
                &HashSet::new(),
                &HashSet::new(),
                1_001,
            )
            .unwrap()
            .counted;
            assert_eq!(usage.lifetime().as_str(), lifetime, "{state:?}");
            assert_eq!(
                (usage.reserved(), usage.spent(), usage.released()),
                (reserved, spent, released),
                "{state:?}"
            );
        }
        rail.next(crate::rail::Behaviour::Hang);
        let outcome = rail
            .pay_outside(
                PaymentEndpointIdentifier::new("lightning").unwrap(),
                PaymentEndpointPayload::new("ln-test").unwrap(),
                PaymentAmount {
                    asset: terms.asset.clone(),
                    value,
                },
                id,
            )
            .await
            .unwrap();
        assert_eq!(outcome, RailOutcome::Pending);
        let in_flight = wallet_inventory(&rail, payer).await.unwrap();
        assert_eq!(in_flight[0].outcome, RailOutcome::Pending);
        clock.advance_ms(1);
        rail.settle(&id, true);
        let history = wallet_inventory(&rail, payer).await.unwrap();
        let RailOutcome::Paid(paid) = &history[0].outcome else {
            panic!("settled")
        };
        assert_eq!(paid.paid_at_ms, Some(18));
        let claims = HashMap::from([(
            id,
            ReservationClaim {
                obligation,
                state: ClaimState::Open,
            },
        )]);
        let Accounting {
            counted,
            unclassified,
            ..
        } = read_usage(&rail, payer, &terms, &ledger, &claims, 1_001)
            .await
            .unwrap();
        assert!(unclassified.is_empty());
        assert_eq!(counted.lifetime().as_str(), "70");
        // 10 more reserved that the history doesn't show count beside the paid 70.
        let later = AttemptId::from_bytes([9; 16]);
        let mut both = ledger.clone();
        both.entries.push(LedgerEntry {
            obligation: [4; 32],
            attempt_id: later,
            value: Decimal::new("10").unwrap(),
            asset: terms.asset.clone(),
            at: 1_001,
            outcome: None,
        });
        let mut two = claims.clone();
        two.insert(
            later,
            ReservationClaim {
                obligation: [4; 32],
                state: ClaimState::Open,
            },
        );
        let mixed = read_usage(&rail, payer, &terms, &both, &two, 1_001)
            .await
            .unwrap();
        assert_eq!(mixed.counted.lifetime().as_str(), "80");
        assert_eq!(mixed.counted.reserved_amount().as_str(), "10");
        assert_eq!(
            check_limits(&terms, &counted, &Decimal::new("31").unwrap()),
            Err(LimitKind::Lifetime)
        );
        assert!(check_limits(&terms, &counted, &Decimal::new("30").unwrap()).is_ok());
        assert!(
            read_usage(&rail, payer, &terms, &ledger, &HashMap::new(), 1_001)
                .await
                .is_none()
        );
        rail.forget_history(&id);
        assert!(read_usage(&rail, payer, &terms, &ledger, &claims, 1_001)
            .await
            .is_none());
        assert_eq!(rail.executions(&id), 1);
    }
}
