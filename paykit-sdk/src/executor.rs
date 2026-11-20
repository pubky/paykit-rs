//! Same-executor accounting after a device restore (Paykit spec 8.3).

mod approval;
mod checkpoint;
mod compaction;
mod evidence;
mod grant;
mod ledger;
mod restore;
pub(crate) use checkpoint::ExecutorHistory;
pub(crate) use compaction::LedgerCompaction;

use std::collections::{HashMap, HashSet};

use paykit_lib::allowances::{usage, AllowanceTerms, AttemptEvidence, ClaimState, Ledger, Usage};
use paykit_lib::AttemptId;

use crate::rail::{AllowanceDispatch, Rail, RailOutcome};

/// The proposal must belong to the Allower's surviving history as of the grant.
fn in_grant_history(proposed: u64, grant_generation: u64, recovery_from: Option<u64>) -> bool {
    proposed > recovery_from.unwrap_or(0) && proposed <= grant_generation
}

/// The verified opening claim for an Allowance reservation. Its signed batch, occurrence
/// and attempt must agree with the ledger operation before it reaches accounting.
#[derive(Clone, Copy)]
struct ReservationClaim {
    obligation: [u8; 32],
    state: ClaimState,
}

/// A complete wallet inventory can still contain an unclassified payment. Its
/// original attempt must be attributed before a missing service entry is an alarm.
struct Accounting {
    counted: Usage,
    unclassified: Vec<AllowanceDispatch>,
}

async fn wallet_inventory(rail: &dyn Rail, payer: [u8; 32]) -> Option<Vec<AllowanceDispatch>> {
    let declaration = rail.declaration();
    if !declaration.history || !declaration.history_survives_restore {
        return None;
    }
    rail.allowance_history(payer).await.ok()?
}

fn reconciled_usage(
    inventory: Vec<AllowanceDispatch>,
    terms: &AllowanceTerms,
    ledger: &Ledger,
    claims: &HashMap<AttemptId, ReservationClaim>,
    at: u64,
) -> Option<Accounting> {
    let mut unclassified = Vec::new();
    let mut seen = HashSet::new();
    let mut evidence = HashMap::new();
    for entry in &ledger.entries {
        if let Some(claim) = claims.get(&entry.attempt_id) {
            if claim.obligation != entry.obligation {
                return None;
            }
            evidence.insert(entry.attempt_id, AttemptEvidence::from(claim.state));
        }
    }
    for dispatch in inventory {
        let id = dispatch.attempt_id;
        if !seen.insert(id) {
            return None;
        }
        let entry = ledger.entries.iter().find(|e| e.attempt_id == id);
        let failed = ledger.failed.iter().find(|f| f.attempt_id == id);
        let member = entry
            .map(|e| (&e.obligation, &e.asset, &e.value))
            .or_else(|| failed.map(|f| (&f.obligation, &terms.asset, &f.value)));
        let Some((obligation, asset, value)) = member else {
            unclassified.push(dispatch);
            continue;
        };
        let claim = claims.get(&id)?;
        if *obligation != claim.obligation
            || asset != &dispatch.amount.asset
            || value != &dispatch.amount.value
        {
            return None;
        }
        if failed.is_some() {
            match &dispatch.outcome {
                RailOutcome::Failed
                    if matches!(claim.state, ClaimState::Failed { by_owner: true }) => {}
                RailOutcome::Paid(_) => unclassified.push(dispatch),
                _ => return None,
            }
            // The ledger already counts this failure in full. Its method history cannot release it.
            continue;
        }
        evidence.get_mut(&id)?.paid = matches!(dispatch.outcome, RailOutcome::Paid(_));
    }
    Some(Accounting {
        counted: usage(terms, ledger, &evidence, at).ok()?,
        unclassified,
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
    use pubky_switch_client::clock::TestClock;
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
            at,
        )
    }
    #[test]
    fn test_compacted_method_failure_counts_once_and_requires_its_original_claim() {
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
        let dispatch = AllowanceDispatch {
            attempt_id: id,
            amount: PaymentAmount {
                asset: terms.asset.clone(),
                value,
            },
            outcome: RailOutcome::Failed,
        };
        let read = |dispatch, claims: &HashMap<_, _>| {
            reconciled_usage(vec![dispatch], &terms, &ledger, claims, 100_000_000)
        };
        let accounting = read(dispatch.clone(), &claims).unwrap();
        assert_eq!(accounting.counted.lifetime().as_str(), "70");
        assert!(accounting.unclassified.is_empty());
        assert!(read(dispatch.clone(), &HashMap::new()).is_none());
        let mut different = dispatch.clone();
        different.amount.value = Decimal::new("69").unwrap();
        assert!(read(different, &claims).is_none());
        let mut pending = dispatch;
        pending.outcome = RailOutcome::Pending;
        assert!(read(pending, &claims).is_none());
    }

    #[tokio::test]
    async fn test_account_history_charges_the_open_seventy_after_restore() {
        // The wallet clock runs behind the Switch that dated the reservation at 1,001.
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
        for (state, reserved, spent) in [
            (Some(ClaimState::Open), 1, 0),
            (Some(ClaimState::Unknown), 1, 0),
            (Some(ClaimState::Done), 0, 1),
            (Some(ClaimState::Failed { by_owner: true }), 0, 1),
            (None, 0, 1),
        ] {
            let claims = state
                .map(|state| (id, ReservationClaim { obligation, state }))
                .into_iter()
                .collect();
            let usage = reconciled_usage(Vec::new(), &terms, &ledger, &claims, 1_001)
                .unwrap()
                .counted;
            assert_eq!(usage.lifetime().as_str(), "70");
            assert_eq!(
                (usage.reserved(), usage.spent(), usage.released()),
                (reserved, spent, 0)
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
        } = read_usage(&rail, payer, &terms, &ledger, &claims, 1_001)
            .await
            .unwrap();
        assert!(unclassified.is_empty());
        assert_eq!(counted.lifetime().as_str(), "70");
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
