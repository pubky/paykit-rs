//! `allowance_usage.json`: Allowance usage for every limit kind, the outcome table,
//! compaction (Paykit spec 8.3, Paykit wire 5.5), and the period limits terms take or refuse
//! as duplicates (Paykit wire 4.15).

use std::collections::HashMap;

use paykit_lib::allowances::{
    check_limits, check_successor, compact, usage, AllowanceTerms, AttemptEvidence, ClaimState,
    EntryOutcome, Ledger, Usage,
};
use paykit_lib::amount::Decimal;
use paykit_lib::identifiers::AttemptId;
use serde_json::{json, Value};

use crate::common::{arr, generated, paykit, pubky, quoted, st, time, writing, Keys, Place, BOB};

fn usage_json(u: &Usage) -> Value {
    json!({
        "lifetime": u.lifetime().as_str(),
        "periods": u.periods().iter().map(|p| json!({ "amount": p.amount().as_str(), "count": p.count() })).collect::<Vec<_>>(),
        "reserved": u.reserved(),
        "spent": u.spent(),
        "released": u.released(),
    })
}

fn claims(case: &Value) -> HashMap<AttemptId, AttemptEvidence> {
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
            (AttemptId::parse(st(c, "attempt_id")).unwrap(), state.into())
        })
        .collect()
}

fn ledger(v: &Value) -> Ledger {
    Ledger::from_map(v.as_object().unwrap()).unwrap()
}

#[test]
fn test_vectors_allowance_usage() {
    if !writing() {
        check();
    }
}

/// `allowance_usage.json` against paykit-lib: its generated cases, then every case's verdict.
/// With [`WRITE_VECTORS`](crate::common::WRITE_VECTORS) set, the regeneration driver runs it
/// once it has written the set.
pub(crate) fn check() {
    generate(&Keys::from_seeds());
    let v = paykit("allowance_usage.json");
    let cases = arr(&v, "cases");
    assert_eq!(cases.len(), 9);
    for case in cases {
        let name = st(case, "name");
        let terms = AllowanceTerms::from_json(&case["terms"]).unwrap();
        assert_eq!(
            terms.to_json().unwrap(),
            case["terms"],
            "{name}: terms write back"
        );
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
        assert_eq!(u.lifetime().as_str(), st(c, key), "{key}");
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
    let failures = arr(&v, "failure_evidence");
    assert_eq!(failures.len(), failure_cases().len());
    for case in failures {
        check_failure_case(case);
    }
    check_spellings(&v);
    for case in arr(&v["period_limit_members"], "refused") {
        let name = st(case, "name");
        let error = AllowanceTerms::from_json(&case["terms"]).expect_err(name);
        assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
    }
    assert_eq!(arr(&v["period_limit_members"], "refused").len(), 2);
}

/// The top-level description, which names every member.
const DESCRIPTION: &str =
    "Allowance usage (wire 5.5, spec 8.3 with specs/allowances.md): for every period limit the \
amount and count of counted entries in the period containing t (anchored) or with t < at + \
60 s + L (rolling), the lifetime amount including compacted totals, and entry counts \
reserved, spent and released. A ledger or claim done, or a validated payer proof, spends. \
Open and unknown claims reserve entries without an outcome. Every failed attempt spends \
immediately, whoever wrote the failure, while its claim remains and after it is gone. \
Only durable journal evidence bound to the attempt proving it ended before the payment \
method's call and can never dispatch releases it; a restored journal, a reported failure \
or user confirmation alone is insufficient. Done or a paid proof overrides release. An \
entry without an outcome whose claim is gone spends. Compacted totals.failed items use \
the same rule for lifetime usage. Candidates are checked against every limit; refused_by \
names the first limit that refuses. failure_evidence holds pure counting inputs: evidence \
means already validated local journal and payer-proof evidence, not a shared record member; \
SDK checks bind it to the attempt, obligation, Allowance, amount, asset and owner. Its \
compacted cases replace the entry with one totals.failed item. period_limit_spellings \
checks duplicate terms by wire spelling; period_limit_members checks required members.";

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
/// generated"), compared with the file, and written into the set by the
/// regeneration driver (`regenerate.rs`).
pub(crate) fn generate(_keys: &Keys) {
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
    let mut cases = vec![
        (Place::Member("/description"), json!(DESCRIPTION)),
        (
            Place::Member("/period_limit_spellings"),
            json!({ "accepted": accepted, "description": SPELLINGS_DESCRIPTION, "refused": refused }),
        ),
        (
            Place::Member("/period_limit_members"),
            period_limit_members(),
        ),
    ];
    cases.push((Place::Member("/failure_evidence"), json!(failure_cases())));
    for (index, name) in [
        (
            3,
            "outcomes: open and unknown reserve; failed attempts spend; missing claims spend",
        ),
        (
            6,
            "failures by either app spend; done overrides a failed ledger outcome",
        ),
        (
            7,
            "compacted failed attempts spend while claims remain and after they are gone",
        ),
    ] {
        cases.push((Place::At(format!("/cases/{index}/name")), json!(name)));
    }
    cases.extend(computed());
    generated("allowance_usage.json", &cases);
}

/// Counting evidence is an input to the pure library, not a shared wire member.
fn failure_cases() -> Vec<Value> {
    use ClaimState::{Done, Failed, Open, Unknown};
    use EntryOutcome::{Done as Paid, Failed as Unpaid};
    let v = paykit("allowance_usage.json");
    let mut terms = v["cases"][0]["terms"].clone();
    terms["period_limits"] = json!([]);
    terms["lifetime_amount_limit"] = json!("0.01");
    let mut entry = v["cases"][3]["ledger"]["entries"][0].clone();
    entry["value"] = json!("0.01");
    let cases = [
        (
            "owner failed",
            Some(Failed { by_owner: true }),
            None,
            false,
            false,
            false,
            false,
        ),
        (
            "another app failed",
            Some(Failed { by_owner: false }),
            None,
            false,
            false,
            false,
            false,
        ),
        (
            "failed ledger, claim gone",
            None,
            Some(Unpaid),
            false,
            false,
            false,
            false,
        ),
        (
            "unknown dispatch",
            Some(Unknown),
            None,
            false,
            false,
            false,
            false,
        ),
        ("open attempt", Some(Open), None, false, false, false, false),
        (
            "no outcome, claim gone",
            None,
            None,
            false,
            false,
            false,
            false,
        ),
        (
            "never dispatched, owner failed",
            Some(Failed { by_owner: true }),
            None,
            true,
            false,
            false,
            true,
        ),
        (
            "never dispatched, failed ledger, claim gone",
            None,
            Some(Unpaid),
            true,
            false,
            false,
            true,
        ),
        (
            "paid proof overrides never dispatched",
            None,
            Some(Unpaid),
            true,
            true,
            false,
            false,
        ),
        (
            "done claim overrides never dispatched",
            Some(Done),
            Some(Unpaid),
            true,
            false,
            false,
            false,
        ),
        (
            "done ledger overrides never dispatched",
            None,
            Some(Paid),
            true,
            false,
            false,
            false,
        ),
        (
            "compacted owner failure",
            Some(Failed { by_owner: true }),
            Some(Unpaid),
            false,
            false,
            true,
            false,
        ),
        (
            "compacted failure, claim gone",
            None,
            Some(Unpaid),
            false,
            false,
            true,
            false,
        ),
        (
            "compacted never dispatched",
            None,
            Some(Unpaid),
            true,
            false,
            true,
            true,
        ),
        (
            "compacted paid proof overrides never dispatched",
            None,
            Some(Unpaid),
            true,
            true,
            true,
            false,
        ),
        (
            "compacted done claim overrides never dispatched",
            Some(Done),
            Some(Unpaid),
            true,
            false,
            true,
            false,
        ),
    ];
    cases.into_iter().map(|(name, claim, outcome, never_dispatched, paid, compacted, released)| {
        let mut entry = entry.clone();
        match outcome {
            Some(Paid) => entry["outcome"] = json!("done"),
            Some(Unpaid) => entry["outcome"] = json!("failed"),
            None => entry["outcome"] = Value::Null,
        }
        let claims = match claim {
            None => json!([]),
            Some(state) => {
                let mut c = json!({ "attempt_id": entry["attempt_id"], "state": match state {
                    Open => "open", Unknown => "unknown", Done => "done", Failed { .. } => "failed",
                }});
                if let Failed { by_owner } = state { c["resolved_by_owner"] = json!(by_owner); }
                json!([c])
            }
        };
        let mut l = v["cases"][0]["ledger"].clone();
        l["totals"] = json!({ "spent": "", "count": 0, "failed": [] });
        l["entries"] = json!([entry]);
        if compacted {
            l["entries"] = json!([]);
            l["totals"]["failed"] = json!([{ "attempt_id": entry["attempt_id"], "obligation": entry["obligation"], "value": entry["value"] }]);
        }
        let evidence = AttemptEvidence { claim, paid, never_dispatched };
        let attempt = AttemptId::parse(st(&entry, "attempt_id")).unwrap();
        let u = usage(&AllowanceTerms::from_json(&terms).unwrap(), &ledger(&l), &HashMap::from([(attempt, evidence)]), time(st(&v["cases"][3], "t"))).unwrap();
        // Independent exact expected amount prevents regeneration blessing an undercount.
        assert_eq!(u.lifetime().as_str(), if released { "0" } else { "0.01" }, "{name}");
        json!({ "name": name, "terms": terms, "ledger": l, "claims": claims,
            "t": v["cases"][3]["t"], "attempt_id": entry["attempt_id"],
            "evidence": { "never_dispatched": never_dispatched, "paid": paid },
            "usage": usage_json(&u), "candidate": { "amount": "0.01", "allowed": released } })
    }).collect()
}

fn check_failure_case(case: &Value) {
    let terms = AllowanceTerms::from_json(&case["terms"]).unwrap();
    let mut evidence = claims(case);
    let counted = evidence
        .entry(AttemptId::parse(st(case, "attempt_id")).unwrap())
        .or_default();
    counted.never_dispatched = case["evidence"]["never_dispatched"].as_bool().unwrap();
    counted.paid = case["evidence"]["paid"].as_bool().unwrap();
    let u = usage(
        &terms,
        &ledger(&case["ledger"]),
        &evidence,
        time(st(case, "t")),
    )
    .unwrap();
    assert_eq!(usage_json(&u), case["usage"], "{}", st(case, "name"));
    assert_eq!(
        check_limits(
            &terms,
            &u,
            &Decimal::parse(st(&case["candidate"], "amount")).unwrap()
        )
        .is_ok(),
        case["candidate"]["allowed"].as_bool().unwrap(),
        "{}",
        st(case, "name")
    );
}

/// Each case's usage and its candidates' verdicts, and the compaction's result, from their
/// authored terms, ledgers, claims, times and amounts.
fn computed() -> Vec<(Place<'static>, Value)> {
    let v = paykit("allowance_usage.json");
    let mut out = Vec::new();
    for (i, case) in arr(&v, "cases").iter().enumerate() {
        let terms = AllowanceTerms::from_json(&case["terms"]).unwrap();
        let u = usage(
            &terms,
            &ledger(&case["ledger"]),
            &claims(case),
            time(st(case, "t")),
        )
        .unwrap();
        out.push((Place::At(format!("/cases/{i}/usage")), usage_json(&u)));
        for (j, candidate) in arr(case, "candidates").iter().enumerate() {
            let amount = st(candidate, "amount");
            let verdict = match check_limits(&terms, &u, &Decimal::parse(amount).unwrap()) {
                Ok(()) => json!({ "allowed": true, "amount": amount }),
                Err(by) => json!({ "allowed": false, "amount": amount, "refused_by": by.as_str() }),
            };
            out.push((Place::At(format!("/cases/{i}/candidates/{j}")), verdict));
        }
    }
    let c = &v["compaction"];
    let terms = AllowanceTerms::from_json(&c["terms"]).unwrap();
    let now = time(st(c, "now"));
    let before = ledger(&c["before"]);
    let after = compact(&terms, &before, now).unwrap();
    out.push((
        Place::At("/compaction/changed".into()),
        json!(after.is_some()),
    ));
    let after = after.unwrap_or(before.clone());
    let written: Value = serde_json::from_slice(&after.to_json().unwrap()).unwrap();
    out.push((Place::At("/compaction/after".into()), written));
    let claim_map = claims(c);
    for (l, key) in [(&before, "lifetime_before"), (&after, "lifetime_after")] {
        let u = usage(&terms, l, &claim_map, now).unwrap();
        out.push((
            Place::At(format!("/compaction/{key}")),
            json!(u.lifetime().as_str()),
        ));
    }
    out
}

const MEMBERS_DESCRIPTION: &str = "Wire 4.15, specs/allowances.md: the terms of the first \
case with its period limit's amount_limit or payment_count_limit left out. A period limit \
carries both, each possibly null, not both null; a missing one isn't read as null, so every \
reader refuses these terms. reason is paykit-lib's wording.";

/// `period_limit_members`, as paykit-lib generates it.
fn period_limit_members() -> Value {
    let base = paykit("allowance_usage.json")["cases"][0]["terms"].clone();
    let refused: Vec<Value> = [
        ("a period limit without amount_limit", "amount_limit"),
        (
            "a period limit without payment_count_limit",
            "payment_count_limit",
        ),
    ]
    .into_iter()
    .map(|(name, member)| {
        let mut terms = base.clone();
        terms["period_limits"][0]
            .as_object_mut()
            .unwrap()
            .remove(member);
        let error = AllowanceTerms::from_json(&terms).expect_err(name);
        json!({ "name": name, "reason": quoted(&error.to_string()), "terms": terms })
    })
    .collect();
    json!({ "description": MEMBERS_DESCRIPTION, "refused": refused })
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
        assert_eq!(
            terms.to_json().unwrap(),
            case["terms"],
            "{name}: terms write back"
        );
        assert_eq!(
            AllowanceTerms::from_json(&terms.to_json().unwrap()).unwrap(),
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
