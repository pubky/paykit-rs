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
    check_successor(&terms, &before, now, &after, now).unwrap();
    let early = time("2026-01-10T00:00:00.000Z");
    assert!(check_successor(&terms, &before, early, &after, early).is_err());
    // An entry of another asset makes the ledger invalid: usage and compaction refuse it.
    let mut other = c["before"].clone();
    other["entries"][0]["asset"] = json!("usd");
    let other = ledger(&other);
    assert!(usage(&terms, &other, &claim_map, now).is_err());
    assert!(compact(&terms, &other, now).is_err());
    check_successor_cases(&v);
    let failures = arr(&v, "failure_evidence");
    assert_eq!(failures.len(), failure_cases(&v).len());
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
Open and unknown claims reserve entries without an outcome. Every failed attempt is \
released, whoever wrote the failure, while its claim remains and after it is gone: it \
counts in no amount and no count. Done or a paid proof overrides the release and spends. \
An entry without an outcome whose claim is gone spends. Compacted totals.failed items use \
the same rule for lifetime usage. Candidates are checked against every limit; refused_by \
names the first limit that refuses. failure_evidence holds pure counting inputs: evidence \
means already validated payer-proof evidence (paid), not a shared record member, and the \
failure itself comes from the claim, the entry or the compacted item; \
Callers must bind it to the attempt, obligation, Allowance, amount, asset and owner. Its \
compacted cases replace the entry with one totals.failed item. period_limit_spellings \
checks duplicate terms by wire spelling; period_limit_members checks required members. \
compaction/successors checks which newer ledger versions a reader holding an older one \
accepts after a compaction it didn't see.";

const SUCCESSORS_DESCRIPTION: &str = "Wire 5.5 From one version to the next, spec 8.3: a \
reader holds held, written at service time held_time, and reads newer, written at newer_time, \
which no longer lists a held entry. newer counts that entry in its totals, as a compaction \
writes it. The reader accepts newer when the entry's at + longest period + 60 s was before \
some service time from held_time to newer_time, the longest period taken at that time, so \
an anchored month or year limit's entry compacted in a shorter period is accepted in a \
version written in a longer one. A held_time after newer_time judges at newer_time alone. \
accepted is the verdict.";

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
pub(crate) fn generate(keys: &Keys) {
    let mut inputs = paykit("allowance_usage.json");
    let q = pubky_dispatch_core::enc::b64(
        &crate::sealed::stored_message(keys, "allowance_acceptance").hash(),
    );
    for case in inputs["cases"].as_array_mut().unwrap() {
        case["ledger"]["acceptance"] = json!(q);
    }
    inputs["compaction"]["before"]["acceptance"] = json!(q);
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
    cases.push((
        Place::Member("/failure_evidence"),
        json!(failure_cases(&inputs)),
    ));
    cases.push((
        Place::Member("/compaction/successors"),
        successor_cases(&inputs),
    ));
    for (index, name) in [
        (
            3,
            "outcomes: open and unknown reserve; failed attempts are released; missing claims spend",
        ),
        (
            6,
            "failures by either app are released; done overrides a failed ledger outcome",
        ),
        (
            7,
            "compacted failed attempts are released while claims remain and after they are gone",
        ),
    ] {
        cases.push((Place::At(format!("/cases/{index}/name")), json!(name)));
    }
    cases.extend(computed(&inputs));
    generated("allowance_usage.json", &cases);
}

/// `compaction/successors`: a held ledger with one `done` entry, the version a compaction at
/// `compacted_at` writes from it, and the verdict of a reader holding it from `held_time` on
/// that version written at `newer_time`.
fn successor_cases(v: &Value) -> Value {
    let c = &v["compaction"];
    let anchored = |unit: &str| {
        let mut terms = c["terms"].clone();
        terms["period_limits"][0]["period"]["anchor"] = json!("2026-01-01T00:00:00.000Z");
        terms["period_limits"][0]["period"]["unit"] = json!(unit);
        terms
    };
    let monthly = anchored("month");
    let yearly = anchored("year");
    // (name, terms, entry at, compacted at, held_time, newer_time)
    let inputs = [
        (
            "monthly: compacted at February's end, written in March",
            &monthly,
            "2026-01-31T12:00:00.000Z",
            "2026-02-28T23:59:00.000Z",
            "2026-01-31T13:00:00.000Z",
            "2026-03-01T00:00:30.000Z",
        ),
        (
            "monthly: held from the newer write's time, judged in March alone",
            &monthly,
            "2026-01-31T12:00:00.000Z",
            "2026-02-28T23:59:00.000Z",
            "2026-03-01T00:00:30.000Z",
            "2026-03-01T00:00:30.000Z",
        ),
        (
            "monthly: held from after the newer write's time, judged at newer_time alone",
            &monthly,
            "2026-01-31T12:00:00.000Z",
            "2026-02-28T23:59:00.000Z",
            "2026-03-05T00:00:00.000Z",
            "2026-03-01T00:00:30.000Z",
        ),
        (
            "monthly: no time from held_time to newer_time could compact the entry",
            &monthly,
            "2026-02-01T00:30:00.000Z",
            "2026-04-15T00:00:00.000Z",
            "2026-02-02T00:00:00.000Z",
            "2026-03-01T01:00:00.000Z",
        ),
        (
            "yearly: compacted at 2027's end, written in leap year 2028",
            &yearly,
            "2026-12-31T12:00:00.000Z",
            "2027-12-31T23:00:00.000Z",
            "2026-12-31T13:00:00.000Z",
            "2028-01-01T00:00:30.000Z",
        ),
        (
            "yearly: held from the newer write's time, judged in 2028 alone",
            &yearly,
            "2026-12-31T12:00:00.000Z",
            "2027-12-31T23:00:00.000Z",
            "2028-01-01T00:00:30.000Z",
            "2028-01-01T00:00:30.000Z",
        ),
    ];
    let cases: Vec<Value> = inputs
        .iter()
        .map(|(name, terms, at, compacted_at, held_time, newer_time)| {
            let mut held = c["before"].clone();
            let mut entry = held["entries"][0].clone();
            entry["at"] = json!(at);
            held["entries"] = json!([entry]);
            let read = AllowanceTerms::from_json(terms).unwrap();
            let newer = compact(&read, &ledger(&held), time(compacted_at))
                .unwrap()
                .unwrap_or_else(|| panic!("{name}: the compaction drops the entry"));
            let accepted = check_successor(
                &read,
                &ledger(&held),
                time(held_time),
                &newer,
                time(newer_time),
            )
            .is_ok();
            let newer: Value = serde_json::from_slice(&newer.to_json().unwrap()).unwrap();
            json!({ "accepted": accepted, "held": held, "held_time": held_time, "name": name,
                "newer": newer, "newer_time": newer_time, "terms": terms })
        })
        .collect();
    json!({ "cases": cases, "description": SUCCESSORS_DESCRIPTION })
}

/// `compaction/successors` against paykit-lib: every case's verdict.
fn check_successor_cases(v: &Value) {
    let cases = arr(&v["compaction"]["successors"], "cases");
    assert_eq!(cases.len(), 6);
    for case in cases {
        let name = st(case, "name");
        let terms = AllowanceTerms::from_json(&case["terms"]).unwrap();
        let verdict = check_successor(
            &terms,
            &ledger(&case["held"]),
            time(st(case, "held_time")),
            &ledger(&case["newer"]),
            time(st(case, "newer_time")),
        );
        assert_eq!(
            verdict.is_ok(),
            case["accepted"].as_bool().unwrap(),
            "{name}: {verdict:?}"
        );
    }
}

/// Counting evidence is an input to the pure library, not a shared wire member.
fn failure_cases(v: &Value) -> Vec<Value> {
    use ClaimState::{Done, Failed, Open, Unknown};
    use EntryOutcome::{Done as Paid, Failed as FailedOutcome};
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
            true,
        ),
        (
            "another app failed",
            Some(Failed { by_owner: false }),
            None,
            false,
            false,
            true,
        ),
        (
            "failed ledger, claim gone",
            None,
            Some(FailedOutcome),
            false,
            false,
            true,
        ),
        ("unknown dispatch", Some(Unknown), None, false, false, false),
        ("open attempt", Some(Open), None, false, false, false),
        ("no outcome, claim gone", None, None, false, false, false),
        (
            "paid proof overrides a failure",
            None,
            Some(FailedOutcome),
            true,
            false,
            false,
        ),
        (
            "done claim overrides a failure",
            Some(Done),
            Some(FailedOutcome),
            false,
            false,
            false,
        ),
        ("done ledger spends", None, Some(Paid), false, false, false),
        (
            "compacted owner failure",
            Some(Failed { by_owner: true }),
            Some(FailedOutcome),
            false,
            true,
            true,
        ),
        (
            "compacted failure, claim gone",
            None,
            Some(FailedOutcome),
            false,
            true,
            true,
        ),
        (
            "compacted paid proof overrides a failure",
            None,
            Some(FailedOutcome),
            true,
            true,
            false,
        ),
        (
            "compacted done claim overrides a failure",
            Some(Done),
            Some(FailedOutcome),
            false,
            true,
            false,
        ),
    ];
    cases.into_iter().map(|(name, claim, outcome, paid, compacted, released)| {
        let mut entry = entry.clone();
        match outcome {
            Some(Paid) => entry["outcome"] = json!("done"),
            Some(FailedOutcome) => entry["outcome"] = json!("failed"),
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
        // The vectors hold no payment-method history (vectors README).
        let evidence = AttemptEvidence {
            claim,
            paid,
            method_pending: false,
        };
        let attempt = AttemptId::parse(st(&entry, "attempt_id")).unwrap();
        let u = usage(&AllowanceTerms::from_json(&terms).unwrap(), &ledger(&l), &HashMap::from([(attempt, evidence)]), time(st(&v["cases"][3], "t"))).unwrap();
        // Independent exact expected amount prevents regeneration blessing an undercount.
        assert_eq!(u.lifetime().as_str(), if released { "0" } else { "0.01" }, "{name}");
        json!({ "name": name, "terms": terms, "ledger": l, "claims": claims,
            "t": v["cases"][3]["t"], "attempt_id": entry["attempt_id"],
            "evidence": { "paid": paid },
            "usage": usage_json(&u), "candidate": { "amount": "0.01", "allowed": released } })
    }).collect()
}

fn check_failure_case(case: &Value) {
    let terms = AllowanceTerms::from_json(&case["terms"]).unwrap();
    let mut evidence = claims(case);
    let counted = evidence
        .entry(AttemptId::parse(st(case, "attempt_id")).unwrap())
        .or_default();
    counted.paid = case["evidence"]["paid"].as_bool().unwrap();
    let u = usage(
        &terms,
        &ledger(&case["ledger"]),
        &evidence,
        time(st(case, "t")),
    )
    .unwrap();
    assert_eq!(usage_json(&u), case["usage"], "{}", st(case, "name"));
    let verdict = check_limits(
        &terms,
        &u,
        &Decimal::parse(st(&case["candidate"], "amount")).unwrap(),
    );
    // These terms hold no period limit and a payment within the per-payment range, so a
    // candidate refused is refused by the lifetime limit, counted from the evidence.
    let expected = match case["candidate"]["allowed"].as_bool().unwrap() {
        true => Ok(()),
        false => Err(paykit_lib::allowances::LimitKind::Lifetime),
    };
    assert_eq!(verdict, expected, "{}", st(case, "name"));
}

/// Each case's usage and its candidates' verdicts, and the compaction's result, from their
/// authored terms, ledgers, claims, times and amounts.
fn computed(v: &Value) -> Vec<(Place<'static>, Value)> {
    let mut out = vec![(
        Place::At("/compaction/before".into()),
        v["compaction"]["before"].clone(),
    )];
    for (i, case) in arr(v, "cases").iter().enumerate() {
        out.push((
            Place::At(format!("/cases/{i}/ledger")),
            case["ledger"].clone(),
        ));
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
