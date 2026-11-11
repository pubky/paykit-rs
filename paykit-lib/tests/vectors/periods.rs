//! `periods.json`: Billing Periods (Paykit spec 8.2, Paykit wire 4.4) and `details_rates`
//! (Paykit wire 4.9).

use paykit_lib::periods::{
    evidence_seconds, payer_may_use_rates, rates_apply, BillingPeriod, PeriodRef, Recurrence,
};
use pubky_switch_core::time::rfc3339;
use serde_json::{json, Value};

use crate::common::{arr, generated, message, paykit, st, time, Place};

fn period_json(p: &BillingPeriod) -> Value {
    json!({ "k": p.index, "starts_at": rfc3339(p.starts_at), "ends_at": rfc3339(p.ends_at),
        "start_secs": p.start_secs() })
}

#[test]
fn test_vectors_periods() {
    generated("periods.json", &details_rates_members());
    let v = paykit("periods.json");
    let cases = arr(&v, "cases");
    assert_eq!(cases.len(), 10);
    for case in cases {
        let name = st(case, "name");
        let rec = Recurrence::from_json(&case["recurrence"]).unwrap();
        assert_eq!(
            rec.to_json(),
            case["recurrence"],
            "{name}: recurrence writes back"
        );
        assert_eq!(
            period_json(&rec.first().unwrap()),
            case["first"],
            "{name}: first"
        );
        assert_eq!(
            rec.last().unwrap().map_or(Value::Null, |p| period_json(&p)),
            case["last"],
            "{name}: last"
        );
        let want = arr(case, "periods");
        let got: Vec<Value> = rec
            .periods(want.len())
            .unwrap()
            .iter()
            .map(period_json)
            .collect();
        assert_eq!(&got, want, "{name}: periods");
        for lookup in arr(case, "lookups") {
            let got = rec.lookup(time(st(lookup, "t"))).unwrap();
            assert_eq!(
                got.map_or(Value::Null, |p| period_json(&p)),
                lookup["period"],
                "{name}: lookup {}",
                st(lookup, "t")
            );
        }
        for named in arr(case, "named") {
            let bp = &named["billing_period"];
            let named_period = PeriodRef {
                starts_at: time(st(bp, "starts_at")),
                ends_at: time(st(bp, "ends_at")),
            };
            assert_eq!(
                rec.named(&named_period).unwrap().is_some(),
                named["valid"].as_bool().unwrap(),
                "{name}: named {bp}"
            );
        }
    }
    check_details_rates(&v);
}

/// The verified payment times of `details_rates`, each with whether the rates apply: at
/// `expires_at`, a millisecond after, four fraction digits truncated, whole seconds; then
/// the start, before `created_at`'s own instant, a millisecond before it, and four fraction
/// digits truncated before it.
const DETAILS_RATES_CASES: [(&str, bool); 7] = [
    ("2026-03-01T00:00:00.500Z", true),
    ("2026-03-01T00:00:00.501Z", false),
    ("2026-03-01T00:00:00.5009Z", true),
    ("2026-03-01T00:00:00Z", true),
    ("2026-02-28T00:00:00.000Z", true),
    ("2026-02-27T23:59:59.999Z", false),
    ("2026-02-27T23:59:59.9999Z", false),
];

/// `details_rates` as paykit-lib generates its rule's members (vectors README "How the files
/// were generated"): the details' `created_at`, the cases and the description. `expires_at`
/// is the file's.
fn details_rates_members() -> Vec<(Place<'static>, Value)> {
    let cases: Vec<Value> = DETAILS_RATES_CASES
        .iter()
        .map(|(t, applies)| json!({ "rates_apply": applies, "verified_payment_time": t }))
        .collect();
    vec![
        (
            Place::Member("/details_rates/created_at"),
            json!("2026-02-28T00:00:00.999Z"),
        ),
        (Place::Member("/details_rates/cases"), Value::Array(cases)),
        (
            Place::Member("/details_rates/description"),
            json!(
                "The rates of period details apply to a payment whose independently verified payment time, from the payment method's evidence, is at or after their start, the details' created_at cut to whole seconds (v0.1's valid_from), and at or before expires_at, all read at millisecond precision with further digits truncated (Switch wire 1.6, wire 4.9; spec 8.2). An evidence time in whole seconds reads as .000. The payee credits a payment at the rates only then; the payer pays by them only while its device clock and its highest verified service time are both at or after their start and before expires_at."
            ),
        ),
    ]
}

fn check_details_rates(v: &Value) {
    let dr = &v["details_rates"];
    let created_at = time(st(dr, "created_at"));
    let expires_at = time(st(dr, "expires_at"));
    let cases = arr(dr, "cases");
    assert_eq!(cases.len(), DETAILS_RATES_CASES.len());
    for case in cases {
        let t = st(case, "verified_payment_time");
        assert_eq!(
            rates_apply(created_at, expires_at, time(t)),
            case["rates_apply"].as_bool().unwrap(),
            "{t}"
        );
    }
    // A block timestamp in whole seconds reads as .000.
    let block = evidence_seconds(1_772_323_200).unwrap();
    assert_eq!(block, time("2026-03-01T00:00:00Z"));
    assert!(rates_apply(created_at, expires_at, block));
    assert!(!rates_apply(
        created_at,
        expires_at,
        evidence_seconds(1_772_323_201).unwrap()
    ));
    // The payer pays by the rates only while both its clocks are from the start and before
    // expires_at.
    let start = time("2026-02-28T00:00:00.000Z");
    assert!(payer_may_use_rates(
        created_at,
        expires_at,
        expires_at - 1,
        expires_at - 1
    ));
    assert!(!payer_may_use_rates(
        created_at,
        expires_at,
        expires_at,
        expires_at - 1
    ));
    assert!(!payer_may_use_rates(
        created_at,
        expires_at,
        expires_at - 1,
        expires_at
    ));
    assert!(payer_may_use_rates(created_at, expires_at, start, start));
    assert!(!payer_may_use_rates(
        created_at,
        expires_at,
        start - 1,
        start
    ));
    assert!(!payer_may_use_rates(
        created_at,
        expires_at,
        start,
        start - 1
    ));
    // messages.json's period details carry expires_at in the writer's form.
    let details: Value =
        serde_json::from_str(st(&message("payment_period_details"), "json")).unwrap();
    let exp = st(&details["body"], "expires_at");
    assert_eq!(rfc3339(time(exp)), exp);
}
