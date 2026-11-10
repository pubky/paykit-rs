//! `allowance_usage.json`: Allowance usage for every limit kind, the outcome table,
//! compaction (Paykit spec 8.3, Paykit wire 5.5), and the period limits terms take or refuse
//! as duplicates (Paykit wire 4.15).

use std::collections::HashMap;

use paykit_lib::allowances::{
    check_limits, check_successor, compact, usage, AllowanceTerms, ClaimState, Ledger, Usage,
};
use paykit_lib::amount::Decimal;
use paykit_lib::identifiers::AttemptId;
use serde_json::{json, Value};

use crate::common::{arr, generated, paykit, pubky, quoted, st, time, Place, BOB};

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
    generate();
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
    check_spellings(&v);
}

/// The top-level description, which names every member.
const DESCRIPTION: &str =
    "Allowance usage (wire 5.5, spec 8.3 with specs/allowances.md): for every period limit the \
amount and count of counted entries in the period containing t (anchored) or with t < at + \
60 s + L (rolling), the lifetime amount including the ledger's compacted totals, and how \
many entries are reserved, spent and released. An entry's own done spends, and its own \
failed releases unless its attempt's claim is now done (which spends); an entry without an \
outcome counts by its attempt's claim state (claims): open and unknown reserve, done \
spends, failed releases only when the attempt's owner wrote it (resolved_by_owner true), a \
failed another app wrote stays reserved until the user releases it, and an entry whose \
claim is gone spends. Candidates are checked against every limit; refused_by names the \
first limit that refuses. A totals.failed item counts like an entry's own failed, in the \
lifetime amount only: it adds its value while its attempt's claim is done, until the \
executor moves it into spent and count. Compaction keeps each compacted failed entry's \
attempt, obligation and value in totals.failed. period_limit_spellings holds terms whose \
period limits are, or aren't, duplicates by their wire spelling (wire 4.15).";

const SPELLINGS_DESCRIPTION: &str = "Wire 4.15, specs/allowances.md: the terms of the first \
case with period_limits replaced. Two period limits are duplicates when every member is \
equal as spelled on the wire: amount_limit and anchor compare as strings, so numerically \
equal amounts (1, 1.0) or one instant spelled two ways are distinct entries; \
payment_count_limit and every compare as integers; kind and unit exactly. accepted are terms \
every reader takes, with each of their period limits; refused are terms with an entry \
twice, which every reader refuses. reason is paykit-lib's wording.";

/// A case of `period_limit_spellings`: its name and its period limits.
type SpellingCase = (&'static str, Value);

/// The period limits of `period_limit_spellings`, accepted then refused.
fn spelling_cases() -> (Vec<SpellingCase>, Vec<SpellingCase>) {
    let rolling = |amount: &str| {
        json!({ "amount_limit": amount, "payment_count_limit": null,
            "period": { "every": 1, "kind": "rolling", "unit": "day" } })
    };
    let anchored = |anchor: &str| {
        json!({ "amount_limit": "0.03", "payment_count_limit": 3,
            "period": { "anchor": anchor, "every": 1, "kind": "anchored", "unit": "month" } })
    };
    let accepted = vec![
        (
            "amount limits 0.03 and 0.030, numerically equal",
            json!([rolling("0.03"), rolling("0.030")]),
        ),
        (
            "anchors naming one instant, spelled with and without a fraction",
            json!([
                anchored("2026-01-31T00:00:00Z"),
                anchored("2026-01-31T00:00:00.000Z")
            ]),
        ),
    ];
    let refused = vec![
        (
            "an amount limit entry twice",
            json!([rolling("0.030"), rolling("0.030")]),
        ),
        (
            "an anchored entry twice",
            json!([
                anchored("2026-01-31T00:00:00Z"),
                anchored("2026-01-31T00:00:00Z")
            ]),
        ),
    ];
    (accepted, refused)
}

/// The terms of a case of `period_limit_spellings`.
fn spelling_terms(limits: &Value) -> Value {
    let mut terms = paykit("allowance_usage.json")["cases"][0]["terms"].clone();
    terms["period_limits"] = limits.clone();
    terms
}

/// The cases paykit-lib generates in `allowance_usage.json` (README "How the files were
/// generated"), compared with the file, and written into it with `PAYKIT_WRITE_VECTORS`.
fn generate() {
    let (accepted, refused) = spelling_cases();
    let accepted: Vec<Value> = accepted
        .iter()
        .map(|(name, limits)| {
            let terms = spelling_terms(limits);
            let read = AllowanceTerms::from_json(&terms).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(read.period_limits.len(), 2, "{name}");
            json!({ "name": name, "terms": terms })
        })
        .collect();
    let refused: Vec<Value> = refused
        .iter()
        .map(|(name, limits)| {
            let terms = spelling_terms(limits);
            let error = AllowanceTerms::from_json(&terms).expect_err(name);
            json!({ "name": name, "reason": quoted(&error.to_string()), "terms": terms })
        })
        .collect();
    generated(
        "allowance_usage.json",
        &[
            (Place::Member("/description"), json!(DESCRIPTION)),
            (
                Place::Member("/period_limit_spellings"),
                json!({ "accepted": accepted, "description": SPELLINGS_DESCRIPTION, "refused": refused }),
            ),
        ],
    );
}

/// `period_limit_spellings` (Paykit wire 4.15): readers take the accepted terms with every
/// period limit and write them back as read, and refuse the others.
fn check_spellings(v: &Value) {
    let m = &v["period_limit_spellings"];
    let (accepted, refused) = spelling_cases();
    assert_eq!(arr(m, "accepted").len(), accepted.len());
    assert_eq!(arr(m, "refused").len(), refused.len());
    for case in arr(m, "accepted") {
        let name = st(case, "name");
        let terms =
            AllowanceTerms::from_json(&case["terms"]).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            terms.period_limits.len(),
            arr(&case["terms"], "period_limits").len(),
            "{name}"
        );
        // Written in the spelling read, they read again to the same entries.
        assert_eq!(terms.to_json(), case["terms"], "{name}: terms write back");
        assert_eq!(
            AllowanceTerms::from_json(&terms.to_json()).unwrap(),
            terms,
            "{name}"
        );
    }
    for case in arr(m, "refused") {
        let name = st(case, "name");
        let error = AllowanceTerms::from_json(&case["terms"]).expect_err(name);
        assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
    }
}
