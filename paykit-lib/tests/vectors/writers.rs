//! paykit-lib's writers against its readers, over Paykit's set: whatever a writer produces,
//! its reader takes, as the same value; and no writer produces an input a rejection case
//! lists, which readers refuse (Paykit wire 4, 5).
//!
//! The values come from the set's messages, records and terms, each changed member by
//! member, so every writer meets both values it must refuse and values it must write.

use paykit_lib::allowances::{AllowanceTerms, Ledger, LimitPeriod};
use paykit_lib::amount::{Asset, Decimal, Rate};
use paykit_lib::bodies::{
    Body, Conversion, PaymentDeadline, PaymentRequest, PeriodDetails, RequestDestination,
};
use paykit_lib::identifiers::{AllowanceId, PaymentEndpointIdentifier, PaymentEndpointPayload};
use paykit_lib::kinds::{check_sender_scopes, targets_app, Kind, Side, OPAQUE_MEMBERS};
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{
    ActivityRecord, ActivitySide, AllowanceRecord, PaymentListRecord, StoredMessage,
};
use paykit_lib::scopes::{check_scopes, permissions_for, verify_certificate, Label};
use pubky_switch_core::message::Message;
use pubky_switch_core::scopes::permission_names;
use serde_json::{json, Value};

use crate::common::{arr, cert, message_json, paykit, pubky, signed, st, Keys, ALICE, BOB};

/// Every message of `messages.json`, parsed, with its body read.
fn messages() -> Vec<(String, Message, Body)> {
    arr(&paykit("messages.json"), "messages")
        .iter()
        .map(|m| {
            let message = Message::parse(st(m, "json").as_bytes(), OPAQUE_MEMBERS).unwrap();
            let body = Body::parse(&message).unwrap();
            (st(m, "name").to_owned(), message, body)
        })
        .collect()
}

/// The property for a body: when the writer writes it, the reader reads the same body back.
/// Returns whether the writer wrote it.
fn body_round_trips(body: &Body, refs: usize, name: &str) -> bool {
    match body.to_body(refs) {
        Ok(map) => {
            let message = Message {
                kind: body.kind().as_str().into(),
                msg_id: "1b4e28ba-2fa1-41d2-883f-0016d3cca427".into(),
                created_at_ms: 0,
                from: pubky_switch_core::message::Sender {
                    identity: [1; 32],
                    app_id: [2; 16],
                },
                to: [3; 32],
                refs: vec![[9; 32]; refs],
                body: map,
            };
            assert_eq!(&Body::parse(&message).unwrap(), body, "{name}");
            true
        }
        Err(error) => {
            assert!(
                matches!(error, paykit_lib::Error::Validation(_)),
                "{name}: {error}"
            );
            false
        }
    }
}

/// Requests changed member by member, from one valid request.
fn request_variants(base: &PaymentRequest) -> Vec<(String, PaymentRequest)> {
    let id = |s: &str| PaymentEndpointIdentifier::parse(s).unwrap();
    let rate = |s: &str| Rate::new(Asset::parse(s).unwrap(), Decimal::parse("1").unwrap()).unwrap();
    let recurrence = paykit_lib::periods::Recurrence::from_json(&json!({
        "anchor": "2026-01-31T00:00:00.000Z", "every": 1, "unit": "month",
        "starts_at": "2026-01-31T00:00:00.000Z", "ends_at": null }))
    .unwrap();
    let mut out: Vec<(String, PaymentRequest)> = Vec::new();
    let destinations = [
        RequestDestination::Any,
        RequestDestination::RequiredApp([7; 16]),
        RequestDestination::Endpoints(Default::default()),
        RequestDestination::Endpoints(
            [(
                id("btc-lightning-bolt11"),
                PaymentEndpointPayload::parse("lnbc1").unwrap(),
            )]
            .into(),
        ),
        RequestDestination::Endpoints(
            [(id("LN"), PaymentEndpointPayload::parse("lnbc1").unwrap())].into(),
        ),
    ];
    let accepted = [
        None,
        Some(Vec::new()),
        Some(vec![id("btc-lightning-bolt11")]),
        Some(vec![id("btc-bitcoin-p2wpkh")]),
        Some(vec![id("btc-lightning-bolt11"), id("btc-lightning-bolt11")]),
    ];
    let conversions = [
        None,
        Some(Conversion::PerPeriod),
        Some(Conversion::Fixed(Vec::new())),
        Some(Conversion::Fixed(vec![rate("btc")])),
        Some(Conversion::Fixed(vec![rate("btc-lightning")])),
        Some(Conversion::Fixed(vec![rate("BTC")])),
        Some(Conversion::Fixed(vec![rate("eth")])),
        Some(Conversion::Fixed(vec![rate("btc"), rate("btc")])),
    ];
    let deadlines = [
        None,
        Some(PaymentDeadline::At(1_767_225_600_000)),
        Some(PaymentDeadline::PeriodStart { seconds: 60 }),
    ];
    for (d, destination) in destinations.iter().enumerate() {
        for (a, accepted) in accepted.iter().enumerate() {
            for (c, conversion) in conversions.iter().enumerate() {
                for (t, deadline) in deadlines.iter().enumerate() {
                    for recurring in [false, true] {
                        for per_period in [false, true] {
                            out.push((
                                format!("d{d} a{a} c{c} t{t} r{recurring} p{per_period}"),
                                PaymentRequest {
                                    destination: destination.clone(),
                                    accepted_payment_endpoint_identifiers: accepted.clone(),
                                    conversion: conversion.clone(),
                                    payment_deadline: *deadline,
                                    recurrence: recurring.then_some(recurrence),
                                    per_period_endpoints: per_period,
                                    ..base.clone()
                                },
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

#[test]
fn test_writers_bodies_write_only_what_their_readers_take() {
    let (mut wrote, mut refused) = (0, 0);
    let mut tally = |ok: bool| if ok { wrote += 1 } else { refused += 1 };
    for (name, message, body) in messages() {
        // Every message of the set writes back as itself.
        assert!(body_round_trips(&body, message.refs.len(), &name), "{name}");
        // ... and only with its kind's refs.
        for refs in 0..4 {
            tally(body_round_trips(&body, refs, &name));
        }
        if let Body::PaymentRequest(request) = &body {
            for (variant, changed) in request_variants(request) {
                tally(body_round_trips(
                    &Body::PaymentRequest(changed),
                    0,
                    &format!("{name} {variant}"),
                ));
            }
        }
        if let Body::PaymentPeriodDetails(details) = &body {
            for rates in [None, Some(Vec::new())] {
                for endpoints in [None, details.payment_endpoints.clone()] {
                    let changed = PeriodDetails {
                        rates: rates.clone(),
                        payment_endpoints: endpoints,
                        ..details.clone()
                    };
                    tally(body_round_trips(
                        &Body::PaymentPeriodDetails(changed),
                        1,
                        &name,
                    ));
                }
            }
        }
        if let Body::ResendRequest(resend) = &body {
            for (msg_ids, period) in [
                (Vec::new(), None),
                (vec!["x".to_owned()], None),
                (
                    resend.msg_ids.clone(),
                    Some(PeriodRef {
                        starts_at: 1,
                        ends_at: 2,
                    }),
                ),
            ] {
                let changed = paykit_lib::bodies::ResendRequest {
                    msg_ids,
                    billing_period: period,
                    accepted_from: None,
                    request_hashes: Vec::new(),
                    ..resend.clone()
                };
                tally(body_round_trips(&Body::ResendRequest(changed), 0, &name));
            }
        }
        if let Body::AllowanceProposal(proposal) = &body {
            let mut twice = proposal.clone();
            twice
                .terms
                .period_limits
                .extend(proposal.terms.period_limits.clone());
            tally(body_round_trips(&Body::AllowanceProposal(twice), 0, &name));
        }
    }
    assert!(
        wrote > 100 && refused > 100,
        "{wrote} written, {refused} refused"
    );
}

#[test]
fn test_writers_terms_and_ledgers_write_only_what_their_readers_take() {
    let v = paykit("allowance_usage.json");
    let mut all_terms: Vec<Value> = arr(&v, "cases")
        .iter()
        .map(|c| c["terms"].clone())
        .collect();
    all_terms.push(v["compaction"]["terms"].clone());
    all_terms.extend(
        arr(&v["period_limit_spellings"], "accepted")
            .iter()
            .map(|c| c["terms"].clone()),
    );
    let (mut wrote, mut refused) = (0, 0);
    for json in all_terms {
        let terms = AllowanceTerms::from_json(&json).unwrap();
        let mut variants = vec![terms.clone()];
        let mut twice = terms.clone();
        twice.period_limits.extend(terms.period_limits.clone());
        variants.push(twice);
        let mut no_ceiling = terms.clone();
        no_ceiling.per_payment_amount = None;
        no_ceiling.lifetime_amount_limit = None;
        no_ceiling.expires_at = None;
        for l in &mut no_ceiling.period_limits {
            l.amount_limit = None;
            l.payment_count_limit = Some(1);
        }
        variants.push(no_ceiling);
        let mut no_limits = terms.clone();
        for l in &mut no_limits.period_limits {
            l.amount_limit = None;
            l.payment_count_limit = None;
        }
        variants.push(no_limits);
        let mut empty_allowed = terms.clone();
        empty_allowed.allowed_payment_endpoint_identifiers = Some(Vec::new());
        variants.push(empty_allowed);
        for variant in variants {
            match variant.to_json() {
                Ok(written) => {
                    assert_eq!(AllowanceTerms::from_json(&written).unwrap(), variant);
                    wrote += 1;
                }
                Err(_) => refused += 1,
            }
        }
        assert!(terms.period_limits.iter().all(|l| matches!(
            l.period,
            LimitPeriod::Anchored(_) | LimitPeriod::Rolling { .. }
        )));
    }
    assert!(wrote > 0 && refused > 0);
    for case in arr(&v, "cases") {
        let ledger = Ledger::from_map(case["ledger"].as_object().unwrap()).unwrap();
        assert_eq!(
            Ledger::from_json(&ledger.to_json().unwrap()).unwrap(),
            ledger
        );
        let mut twice = ledger.clone();
        if let Some(e) = ledger.entries.first() {
            twice.entries.push(e.clone());
            assert!(twice.to_json().is_err(), "an attempt listed twice");
        }
    }
}

#[test]
fn test_writers_records_write_only_what_their_readers_take() {
    let alice = pubky(ALICE);
    let s = crate::sealed::bound_sealed();
    let accepted = arr(&s, "binding_accepted");
    let list = PaymentListRecord::read(st(&accepted[0], "value").as_bytes(), &alice).unwrap();
    let allowance = AllowanceRecord::read(st(&accepted[1], "value").as_bytes(), &alice).unwrap();
    let json = list.to_json().unwrap();
    PaymentListRecord::read(&json, &alice).unwrap();
    let json = allowance.to_json().unwrap();
    AllowanceRecord::read(&json, &alice).unwrap();
    // Each activity record of the set, with members changed one by one: written only when
    // its reader takes it.
    let (mut wrote, mut refused) = (0, 0);
    for case in arr(&s["activity_records"], "accepted") {
        let side = match st(case, "collection") {
            "payer_activity" => ActivitySide::Payer,
            _ => ActivitySide::Payee,
        };
        let own = if side == ActivitySide::Payer {
            alice
        } else {
            pubky(BOB)
        };
        let base = ActivityRecord::read(st(case, "value").as_bytes(), &own, side).unwrap();
        let attempt = paykit_lib::identifiers::AttemptId::from_bytes([5; 16]);
        let mut variants = vec![base.clone()];
        for (i, _) in [0; 7].iter().enumerate() {
            let mut v = base.clone();
            match i {
                0 => v.request_hash = Some([4; 32]),
                1 => {
                    v.billing_period = Some(PeriodRef {
                        starts_at: 1,
                        ends_at: 2,
                    })
                }
                2 => v.attempt_id = Some(attempt),
                3 => {
                    v.allowance_id = Some(AllowanceId::parse(crate::common::ALLOWANCE_ID).unwrap())
                }
                4 => v.attempted_periods = vec![2, 1],
                5 => v.msg_ids = vec!["x".into()],
                _ => v.status = Some("anything".into()),
            }
            variants.push(v);
        }
        for v in variants {
            match v.to_json() {
                Ok(json) => {
                    ActivityRecord::read(&json, &own, side).unwrap();
                    wrote += 1;
                }
                Err(_) => refused += 1,
            }
        }
    }
    assert!(
        wrote > 4 && refused > 4,
        "{wrote} written, {refused} refused"
    );
}

/// The rejection cases of Paykit's set, each refused by the writer that would produce it.
/// Where no paykit-lib writer can produce the input at all (a label that isn't valid, a
/// member a typed writer always writes, a record at another key), its type or the key
/// derivation already rules it out.
#[test]
fn test_writers_refuse_every_rejection_case() {
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let m = paykit("messages.json");
    // conversion_terms: requests and period details a payer drops.
    let parse_body = |json: &str| {
        let message = Message::parse(json.as_bytes(), OPAQUE_MEMBERS).unwrap();
        message.body
    };
    let Body::PaymentRequest(recurring) = Body::parse(
        &Message::parse(&message_json("payment_request_recurring"), OPAQUE_MEMBERS).unwrap(),
    )
    .unwrap() else {
        panic!("a request")
    };
    let rates_of = |body: &serde_json::Map<String, Value>, conversion: bool| -> Vec<Rate> {
        let rates = if conversion {
            &body["conversion"]["rates"]
        } else {
            &body["rates"]
        };
        rates
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                Rate::new(
                    Asset::parse(st(r, "asset")).unwrap(),
                    Decimal::parse(st(r, "value")).unwrap(),
                )
                .unwrap()
            })
            .collect()
    };
    for case in arr(&m["conversion_terms"], "refused") {
        let name = st(case, "name");
        let body = parse_body(st(case, "json"));
        let base =
            Body::parse(&Message::parse(&message_json(st(case, "base")), OPAQUE_MEMBERS).unwrap())
                .unwrap();
        let refused = match base {
            Body::PaymentRequest(base) => PaymentRequest {
                amount: paykit_lib::amount::PaymentAmount {
                    asset: Asset::parse(st(&body["amount"], "asset")).unwrap(),
                    ..base.amount.clone()
                },
                conversion: Some(Conversion::Fixed(rates_of(&body, true))),
                ..base
            }
            .to_body()
            .is_err(),
            Body::PaymentPeriodDetails(base) => PeriodDetails {
                rates: Some(rates_of(&body, false)),
                ..base
            }
            .to_body_for(&recurring)
            .is_err(),
            _ => panic!("{name}: a request or period details"),
        };
        assert!(refused, "{name}: the writer writes it");
    }
    // label_rejections: a label that isn't valid isn't a Label; a request under a label the
    // sender doesn't hold fails the sender's side of check 8; an acceptance isn't wrapped
    // to an app its label leaves out.
    for case in arr(&m["label_rejections"], "cases") {
        let name = st(case, "name");
        let message = Message::parse(st(case, "json").as_bytes(), OPAQUE_MEMBERS).unwrap();
        let label = message.body.get("label").and_then(Value::as_str);
        if label.is_some_and(|l| Label::parse(l).is_err()) {
            continue;
        }
        let sender = keys.app_by_id(st(case, "sender_app"));
        match case.get("recipient_cert") {
            None => assert!(
                check_sender_scopes(&message, cert(sender).scopes()).is_err(),
                "{name}"
            ),
            Some(doc) => {
                let recipient = verify_certificate(&signed(doc), &pubky(BOB)).unwrap();
                let label = label.map(|l| Label::parse(l).unwrap());
                assert!(
                    !targets_app(
                        Kind::PaymentRequestAcceptance,
                        Side::Payer,
                        label.as_ref(),
                        recipient.scopes()
                    ),
                    "{name}"
                );
            }
        }
    }
    // cancellation_roles: a sender wraps no cancellation to the same role.
    for case in arr(&m["cancellation_roles"], "refused") {
        let name = st(case, "name");
        let sender = verify_certificate(&signed(&case["sender_cert"]), &pubky(BOB)).unwrap();
        let recipient = verify_certificate(&signed(&case["recipient_cert"]), &alice).unwrap();
        let side = if sender.scopes().iter().any(|s| s == "pay") {
            Side::Payer
        } else {
            Side::Payee
        };
        assert!(
            !targets_app(
                Kind::PaymentRequestCancellation,
                side,
                None,
                recipient.scopes()
            ),
            "{name}"
        );
    }
    // multi_protocol_senders: an app of another protocol only sends no Paykit kind.
    for case in arr(&m["multi_protocol_senders"], "refused") {
        let name = st(case, "name");
        let message = Message::parse(st(case, "json").as_bytes(), OPAQUE_MEMBERS).unwrap();
        let sender = verify_certificate(&signed(&case["sender_cert"]), &pubky(BOB)).unwrap();
        assert!(
            check_sender_scopes(&message, sender.scopes()).is_err(),
            "{name}"
        );
    }
    // sealed.json activity_records: the records are refused by ActivityRecord::to_json in the
    // generator; the stored messages a sender's certificate doesn't allow aren't stored.
    let s = crate::sealed::bound_sealed();
    for case in arr(&s["activity_records"], "refused") {
        let record: Value = serde_json::from_str(st(case, "value")).unwrap();
        if let Some(request) = record.get("request").filter(|r| !r.is_null()) {
            if case["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("a stored"))
            {
                assert!(
                    StoredMessage::read(request, &alice).is_err(),
                    "{}",
                    case["name"]
                );
            }
        }
    }
    // sealed.json binding_rejections: the record writers refuse a list from another app or
    // another identity than the record names, and an Allowance record whose messages name
    // another Allowance; a record at another key isn't one a writer writes, since it writes
    // at the key the record's own members derive.
    let accepted = arr(&s, "binding_accepted");
    let list = PaymentListRecord::read(st(&accepted[0], "value").as_bytes(), &alice).unwrap();
    let other_app = PaymentListRecord {
        app_id: keys.app("cap").keys.app_id_bytes(),
        ..list.clone()
    };
    assert!(other_app.to_json().is_err());
    let other_payee = PaymentListRecord {
        payee: alice,
        ..list
    };
    assert!(other_payee.to_json().is_err());
    let allowance = AllowanceRecord::read(st(&accepted[1], "value").as_bytes(), &alice).unwrap();
    let rejections = arr(&s, "binding_rejections");
    let other_id = rejections
        .iter()
        .find(|c| st(c, "name") == "allowances: allowance_id names another Allowance, at its key")
        .unwrap();
    let other_id: Value = serde_json::from_str(st(other_id, "value")).unwrap();
    let renamed = AllowanceRecord {
        allowance_id: AllowanceId::parse(st(&other_id, "allowance_id")).unwrap(),
        ..allowance.clone()
    };
    assert!(renamed.to_json().is_err());
    let other_acceptance = rejections
        .iter()
        .find(|c| st(c, "name") == "allowances: an acceptance naming another Allowance")
        .unwrap();
    let other_acceptance: Value = serde_json::from_str(st(other_acceptance, "value")).unwrap();
    let response = StoredMessage::read(&other_acceptance["response"], &pubky(BOB)).unwrap();
    let mixed = AllowanceRecord {
        response: Some(response),
        ..allowance
    };
    assert!(mixed.to_json().is_err());
    // allowance_usage.json: terms with a period limit twice aren't written; a period limit
    // without one of its limits can't be, since the writer writes both.
    for case in arr(
        &paykit("allowance_usage.json")["period_limit_spellings"],
        "refused",
    ) {
        let name = st(case, "name");
        let limits = arr(&case["terms"], "period_limits");
        let mut once = case["terms"].clone();
        once["period_limits"] = json!([limits[0]]);
        let mut terms = AllowanceTerms::from_json(&once).unwrap();
        terms.period_limits.push(terms.period_limits[0].clone());
        assert!(terms.to_json().is_err(), "{name}");
    }
    // documents.json: the scopes and permissions Paykit gives the signer never make a
    // certificate its verifiers refuse: the signer runs the Switch's scope rule and Paykit's,
    // and signs the permissions Paykit's scopes need, beside another protocol's.
    let d = paykit("documents.json");
    let cases = arr(&d["certificate_rejections"], "cases")
        .iter()
        .chain(arr(&d["labelled_scope_rejections"], "cases"))
        .chain(arr(&d["multi_protocol_certificates"], "refused"))
        .chain(arr(&d["executor_certificates"], "refused"))
        .chain(arr(&d["payer_certificates"], "refused"));
    for case in cases {
        let name = st(case, "name");
        let scopes: Vec<String> = serde_json::from_value(case["scopes"].clone()).unwrap();
        let body: Value = serde_json::from_str(st(case, "body")).unwrap();
        let permissions: Vec<String> = serde_json::from_value(body["permissions"].clone()).unwrap();
        let given = permission_names(&permissions_for(&scopes));
        let other_protocol = paykit_lib::scopes::holds_other_protocol(&scopes);
        let covered = given.iter().all(|p| permissions.contains(p))
            && (other_protocol || given == permissions);
        assert!(
            pubky_switch_core::scopes::check_scopes(&scopes).is_err()
                || check_scopes(&scopes).is_err()
                || !covered,
            "{name}: the signer would issue it"
        );
    }
}
