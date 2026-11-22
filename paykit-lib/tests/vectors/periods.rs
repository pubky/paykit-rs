//! `periods.json`: Billing Periods (Paykit spec 8.2, Paykit wire 4.4) and `details_rates`
//! (Paykit wire 4.9).

use paykit_lib::periods::{
    destination_expired, evidence_seconds, payer_may_use_rates, rates_apply, BillingPeriod,
    PeriodRef, Recurrence,
};
use pubky_switch_core::time::rfc3339;
use serde_json::{json, Value};

use crate::common::{arr, generated, message, paykit, st, time, writing, Keys, Place};

fn period_json(p: &BillingPeriod) -> Value {
    json!({ "k": p.index, "starts_at": rfc3339(p.starts_at), "ends_at": rfc3339(p.ends_at),
        "start_secs": p.start_secs() })
}

#[test]
fn test_vectors_periods() {
    if !writing() {
        check();
    }
}

/// `periods.json` against paykit-lib: its generated cases, then the recurrences and the
/// rates. With [`WRITE_VECTORS`](crate::common::WRITE_VECTORS) set, the regeneration driver
/// runs it once it has written the set.
pub(crate) fn check() {
    generate(&Keys::from_seeds());
    let v = paykit("periods.json");
    let cases = arr(&v, "cases");
    assert_eq!(cases.len(), 10);
    for case in cases {
        let rec = Recurrence::from_json(&case["recurrence"]).unwrap();
        assert_eq!(
            rec.to_json(),
            case["recurrence"],
            "{}: recurrence writes back",
            st(case, "name")
        );
    }
    check_details_rates(&v);
    check_destination_expiry(&v);
}

/// The cases paykit-lib generates in `periods.json` (README "How the files were
/// generated"), compared with the file, and written into the set by the regeneration driver
/// (`regenerate.rs`): [`period_members`], [`details_rates_members`] and
/// [`destination_expiry_members`].
pub(crate) fn generate(_keys: &Keys) {
    let mut cases = period_members();
    cases.extend(details_rates_members());
    cases.extend(destination_expiry_members());
    generated("periods.json", &cases);
}

/// What each case of `cases` computes from its authored `recurrence`: its `first` and `last`
/// period, its first periods (as many as the case lists), the period each lookup time falls
/// in, and whether each named period is one of the recurrence's (`valid`).
fn period_members() -> Vec<(Place<'static>, Value)> {
    let v = paykit("periods.json");
    let mut out = Vec::new();
    for (i, case) in arr(&v, "cases").iter().enumerate() {
        let at = |member: String| Place::At(format!("/cases/{i}/{member}"));
        let rec = Recurrence::from_json(&case["recurrence"]).unwrap();
        out.push((at("first".into()), period_json(&rec.first().unwrap())));
        out.push((
            at("last".into()),
            rec.last().unwrap().map_or(Value::Null, |p| period_json(&p)),
        ));
        let periods: Vec<Value> = rec
            .periods(arr(case, "periods").len())
            .unwrap()
            .iter()
            .map(period_json)
            .collect();
        out.push((at("periods".into()), Value::Array(periods)));
        for (j, lookup) in arr(case, "lookups").iter().enumerate() {
            let period = rec.lookup(time(st(lookup, "t"))).unwrap();
            out.push((
                at(format!("lookups/{j}/period")),
                period.map_or(Value::Null, |p| period_json(&p)),
            ));
        }
        for (j, named) in arr(case, "named").iter().enumerate() {
            let bp = &named["billing_period"];
            let named_period = PeriodRef {
                starts_at: time(st(bp, "starts_at")),
                ends_at: time(st(bp, "ends_at")),
            };
            out.push((
                at(format!("named/{j}/valid")),
                json!(rec.named(&named_period).unwrap().is_some()),
            ));
        }
    }
    out
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

/// The details' `created_at` in `details_rates`.
const DETAILS_CREATED_AT: &str = "2026-02-28T00:00:00.999Z";

/// `details_rates` as paykit-lib generates its rule's members (vectors README "How the files
/// were generated"): the details' `created_at`, the cases, each with whether the rates apply
/// as paykit-lib computes it, and the description. `expires_at` is the file's.
fn details_rates_members() -> Vec<(Place<'static>, Value)> {
    let created_at = time(DETAILS_CREATED_AT);
    let expires_at = time(st(&paykit("periods.json")["details_rates"], "expires_at"));
    let cases: Vec<Value> = DETAILS_RATES_CASES
        .iter()
        .map(|(t, expected)| {
            let applies = rates_apply(created_at, expires_at, time(t));
            assert_eq!(applies, *expected, "{t}: the rates apply");
            json!({ "rates_apply": applies, "verified_payment_time": t })
        })
        .collect();
    vec![
        (
            Place::Member("/details_rates/created_at"),
            json!(DETAILS_CREATED_AT),
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

/// The expiry of `destination_expiry`.
const DESTINATION_EXPIRES_AT: &str = "2026-03-01T00:00:00.500Z";

/// The cases of `destination_expiry`: a name, the payer's device clock, the highest service
/// time it has verified (`None` for none), and whether the destination has expired. Each
/// clock at the expiry and a millisecond before it, with the other clock behind, ahead or
/// absent.
const DESTINATION_EXPIRY_CASES: [(&str, &str, Option<&str>, bool); 8] = [
    (
        "device_at_expiry_no_service_time",
        "2026-03-01T00:00:00.500Z",
        None,
        true,
    ),
    (
        "device_before_expiry_no_service_time",
        "2026-03-01T00:00:00.499Z",
        None,
        false,
    ),
    (
        "device_behind_service_at_expiry",
        "2026-03-01T00:00:00.000Z",
        Some("2026-03-01T00:00:00.500Z"),
        true,
    ),
    (
        "device_behind_service_past_expiry",
        "2026-03-01T00:00:00.000Z",
        Some("2026-03-01T00:00:01.000Z"),
        true,
    ),
    (
        "device_behind_service_before_expiry",
        "2026-03-01T00:00:00.000Z",
        Some("2026-03-01T00:00:00.499Z"),
        false,
    ),
    (
        "service_behind_device_at_expiry",
        "2026-03-01T00:00:00.500Z",
        Some("2026-03-01T00:00:00.000Z"),
        true,
    ),
    (
        "service_behind_device_before_expiry",
        "2026-03-01T00:00:00.499Z",
        Some("2026-03-01T00:00:00.000Z"),
        false,
    ),
    (
        "both_clocks_before_expiry",
        "2026-03-01T00:00:00.499Z",
        Some("2026-03-01T00:00:00.499Z"),
        false,
    ),
];

/// The payer's two clocks of a `destination_expiry` case.
fn expiry_clocks(device: &str, service: Option<&str>) -> (u64, Option<u64>) {
    (time(device), service.map(time))
}

/// `destination_expiry` as paykit-lib generates it (vectors README "How the files were
/// generated"): the expiry, the cases, each with whether the destination has expired as
/// paykit-lib computes it, and the description.
fn destination_expiry_members() -> Vec<(Place<'static>, Value)> {
    let expires_at = time(DESTINATION_EXPIRES_AT);
    let cases: Vec<Value> = DESTINATION_EXPIRY_CASES
        .iter()
        .map(|(name, device, service, expected)| {
            let (device_time, service_time) = expiry_clocks(device, *service);
            let expired = destination_expired(expires_at, device_time, service_time);
            assert_eq!(expired, *expected, "{name}: the destination has expired");
            json!({ "device_time": device, "expired": expired, "name": name,
                "service_time": service })
        })
        .collect();
    vec![(
        Place::Member("/destination_expiry"),
        json!({
            "cases": cases,
            "description": "A destination has expired once its expiry is at or before the later of the payer's device clock and the highest service time it has verified for its own account at its current service, compared at millisecond precision (spec 7.2, check the destination; Switch wire 1.6). A null service_time is a payer that has verified none, which judges by its device clock alone. The expiry is a public record's finite expires_at or its updated_at plus the record age limit, a Private Payment List's expires_at or 30 days after its acceptance, or period details' expires_at.",
            "expires_at": DESTINATION_EXPIRES_AT,
        }),
    )]
}

fn check_destination_expiry(v: &Value) {
    let de = &v["destination_expiry"];
    let expires_at = time(st(de, "expires_at"));
    let cases = arr(de, "cases");
    assert_eq!(cases.len(), DESTINATION_EXPIRY_CASES.len());
    for case in cases {
        let name = st(case, "name");
        let service = match &case["service_time"] {
            Value::Null => None,
            Value::String(t) => Some(t.as_str()),
            other => panic!("{name}: service_time is {other}"),
        };
        let (device_time, service_time) = expiry_clocks(st(case, "device_time"), service);
        assert_eq!(
            destination_expired(expires_at, device_time, service_time),
            case["expired"].as_bool().unwrap(),
            "{name}"
        );
    }
}
