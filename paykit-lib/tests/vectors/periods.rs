//! `periods.json`: Billing Periods (Paykit spec 8.2, Paykit wire 4.4) and `details_rates`
//! (Paykit wire 4.9).

use paykit_lib::periods::{
    evidence_seconds, payer_may_use_rates, rates_apply, BillingPeriod, PeriodRef, Recurrence,
};
use pubky_switch_core::time::rfc3339;
use serde_json::{json, Value};

use crate::common::{arr, message, paykit, st, time};

fn period_json(p: &BillingPeriod) -> Value {
    json!({ "k": p.index, "starts_at": rfc3339(p.starts_at), "ends_at": rfc3339(p.ends_at),
        "start_secs": p.start_secs() })
}

#[test]
fn test_vectors_periods() {
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

fn check_details_rates(v: &Value) {
    let dr = &v["details_rates"];
    let expires_at = time(st(dr, "expires_at"));
    let cases = arr(dr, "cases");
    assert_eq!(cases.len(), 4);
    for case in cases {
        let t = st(case, "verified_payment_time");
        assert_eq!(
            rates_apply(expires_at, time(t)),
            case["rates_apply"].as_bool().unwrap(),
            "{t}"
        );
    }
    // A block timestamp in whole seconds reads as .000.
    let block = evidence_seconds(1_772_323_200).unwrap();
    assert_eq!(block, time("2026-03-01T00:00:00Z"));
    assert!(rates_apply(expires_at, block));
    assert!(!rates_apply(
        expires_at,
        evidence_seconds(1_772_323_201).unwrap()
    ));
    // The payer pays by the rates only while both its clocks are before expires_at.
    assert!(payer_may_use_rates(
        expires_at,
        expires_at - 1,
        expires_at - 1
    ));
    assert!(!payer_may_use_rates(expires_at, expires_at, expires_at - 1));
    assert!(!payer_may_use_rates(expires_at, expires_at - 1, expires_at));
    // messages.json's period details carry expires_at in the writer's form.
    let details: Value =
        serde_json::from_str(st(&message("payment_period_details"), "json")).unwrap();
    let exp = st(&details["body"], "expires_at");
    assert_eq!(rfc3339(time(exp)), exp);
}
