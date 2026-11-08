//! `allowance_usage.json`: Allowance usage for every limit kind, the outcome table, and
//! compaction (Paykit spec 8.3, Paykit wire 5.5).

use std::collections::HashMap;

use paykit_lib::allowances::{
    check_limits, check_successor, compact, usage, AllowanceTerms, ClaimState, Ledger, Usage,
};
use paykit_lib::amount::Decimal;
use paykit_lib::identifiers::AttemptId;
use serde_json::{json, Value};

use crate::common::{arr, paykit, pubky, st, time, BOB};

fn usage_json(u: &Usage) -> Value {
    json!({
        "lifetime": u.lifetime.as_str(),
        "periods": u.periods.iter().map(|p| json!({ "amount": p.amount.as_str(), "count": p.count })).collect::<Vec<_>>(),
        "reserved": u.reserved,
        "spent": u.spent,
        "released": u.released,
    })
}

fn claims(case: &Value) -> HashMap<AttemptId, ClaimState> {
    arr(case, "claims")
        .iter()
        .map(|c| {
            let state = match st(c, "state") {
                "open" => ClaimState::Open,
                "unknown" => ClaimState::Unknown,
                "done" => ClaimState::Done,
                "failed" => ClaimState::Failed {
                    by_owner: c["resolved_by_owner"]
                        .as_bool()
                        .expect("a failed claim names its writer"),
                },
                other => panic!("claim state {other}"),
            };
            (AttemptId::parse(st(c, "attempt_id")).unwrap(), state)
        })
        .collect()
}

fn ledger(v: &Value) -> Ledger {
    Ledger::from_map(v.as_object().unwrap()).unwrap()
}

#[test]
fn test_vectors_allowance_usage() {
    let v = paykit("allowance_usage.json");
    let cases = arr(&v, "cases");
    assert_eq!(cases.len(), 9);
    for case in cases {
        let name = st(case, "name");
        let terms = AllowanceTerms::from_json(&case["terms"]).unwrap();
        assert_eq!(terms.to_json(), case["terms"], "{name}: terms write back");
        let l = ledger(&case["ledger"]);
        assert_eq!(l.counterparty, pubky(BOB));
        let back: Value = serde_json::from_slice(&l.to_json().unwrap()).unwrap();
        assert_eq!(back, case["ledger"], "{name}: ledger writes back");
        let u = usage(&terms, &l, &claims(case), time(st(case, "t"))).unwrap();
        assert_eq!(usage_json(&u), case["usage"], "{name}: usage");
        for candidate in arr(case, "candidates") {
            let result = check_limits(
                &terms,
                &u,
                &Decimal::parse(st(candidate, "amount")).unwrap(),
            );
            assert_eq!(
                result.is_ok(),
                candidate["allowed"].as_bool().unwrap(),
                "{name}: {candidate}"
            );
            if let Err(by) = result {
                assert_eq!(
                    by.as_str(),
                    st(candidate, "refused_by"),
                    "{name}: {candidate}"
                );
            }
        }
    }
    // A counterparty that isn't a Pubky ID makes the ledger unreadable.
    let mut bad = cases[0]["ledger"].clone();
    bad["counterparty"] = json!("bob");
    assert!(Ledger::from_map(bad.as_object().unwrap()).is_err());

    let c = &v["compaction"];
    let terms = AllowanceTerms::from_json(&c["terms"]).unwrap();
    let now = time(st(c, "now"));
    let before = ledger(&c["before"]);
    let after = compact(&terms, &before, now).unwrap();
    assert_eq!(after.is_some(), c["changed"].as_bool().unwrap());
    let after = after.unwrap();
    let written: Value = serde_json::from_slice(&after.to_json().unwrap()).unwrap();
    assert_eq!(written, c["after"], "compaction result");
    assert_eq!(after, ledger(&c["after"]));
    let claim_map = claims(c);
    for (l, key) in [(&before, "lifetime_before"), (&after, "lifetime_after")] {
        let u = usage(&terms, l, &claim_map, now).unwrap();
        assert_eq!(u.lifetime.as_str(), st(c, key), "{key}");
    }
    // Compacting again changes nothing, and the step from before to after is accepted.
    assert!(compact(&terms, &after, now).unwrap().is_none());
    check_successor(&terms, &before, &after, now).unwrap();
    assert!(check_successor(&terms, &before, &after, time("2026-01-10T00:00:00.000Z")).is_err());
    // An entry of another asset makes the ledger invalid: usage and compaction refuse it.
    let mut other = c["before"].clone();
    other["entries"][0]["asset"] = json!("usd");
    let other = ledger(&other);
    assert!(usage(&terms, &other, &claim_map, now).is_err());
    assert!(compact(&terms, &other, now).is_err());
}
