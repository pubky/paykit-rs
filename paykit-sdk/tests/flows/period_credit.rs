//! A period's proof names its signed rates; its request decides whether those details
//! also supply the destination (Paykit spec 8.2, Paykit wire 4.9).

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_lib::bodies::{
    Conversion, ListEndpoint, PaymentProof, PeriodDetails, RequestDestination,
};
use paykit_lib::records::{ActivityRecord, ActivitySide};
use paykit_lib::{Asset, AttemptId, Body, Decimal, PaymentAmount, PaymentEndpointPayload, Rate};
use paykit_sdk::rail::{Rail, RailOutcome};
use paykit_sdk::{Paykit, PaykitConfig};
use pubky_switch_client::clock::Clock;

use crate::common::{btc, endpoint, list_version, store, World, HOUR_MS};

const USD: &str = "usd-ach-account";
const BTC: &str = "btc-lightning-bolt11";

#[derive(Clone, Copy, Debug)]
enum Destination {
    Details,
    Request,
    List,
    Public,
}

#[derive(Clone, Copy, Debug)]
enum PaidAt {
    BeforeStart,
    Start,
    BeforeSigned,
    Inside,
    BeforeExpiry,
    Expiry,
    AfterExpiry,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    destination: Destination,
    per_period_endpoints: bool,
    later_period: bool,
    details_endpoints: bool,
    paid_at: PaidAt,
    cross_asset: bool,
    wrong_amount: bool,
    newer_details: bool,
    restored: bool,
}

impl Case {
    fn priced(destination: Destination) -> Self {
        Self {
            destination,
            per_period_endpoints: matches!(destination, Destination::Details),
            later_period: matches!(destination, Destination::Details),
            details_endpoints: matches!(destination, Destination::Details),
            paid_at: PaidAt::Inside,
            cross_asset: true,
            wrong_amount: false,
            newer_details: false,
            restored: false,
        }
    }
}

fn usd(value: &str) -> PaymentAmount {
    PaymentAmount {
        asset: Asset::new("usd").unwrap(),
        value: Decimal::new(value).unwrap(),
    }
}

fn rates(value: &str) -> Vec<Rate> {
    vec![Rate::new(Asset::new("usd").unwrap(), Decimal::new(value).unwrap()).unwrap()]
}

async fn check_credit(case: Case, expected: bool) {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    w.rail.set_fraction_digits(endpoint(USD), Some(2));
    let endpoints: BTreeMap<_, _> = [
        (
            endpoint(BTC),
            PaymentEndpointPayload::new("request-btc").unwrap(),
        ),
        (
            endpoint(USD),
            PaymentEndpointPayload::new("request-usd").unwrap(),
        ),
    ]
    .into_iter()
    .collect();
    let mut draft = super::periods::monthly(w.clock.now_ms(), None)
        .with_conversion(Conversion::PerPeriod)
        .with_accepted_endpoint_identifiers(vec![endpoint(BTC), endpoint(USD)]);
    draft.per_period_endpoints = case.per_period_endpoints;
    let recurrence = draft.recurrence.as_ref().unwrap();
    let first = recurrence.first().unwrap();
    let period: paykit_lib::periods::PeriodRef = if case.later_period {
        recurrence.rule().containing(first.ends_at).unwrap()
    } else {
        first
    }
    .into();
    draft.destination = match case.destination {
        Destination::List | Destination::Public => RequestDestination::Any,
        _ => RequestDestination::Endpoints(endpoints),
    };
    let sent = shop.send_request(w.alice_id(), draft).await.unwrap();
    let request = sent.request.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    if case.later_period {
        w.clock.set_ms(period.starts_at);
    }
    let id = if case.cross_asset { USD } else { BTC };
    let payload = match case.destination {
        Destination::Details => "details-endpoint",
        Destination::Request if case.cross_asset => "request-usd",
        Destination::Request => "request-btc",
        Destination::List => "list-endpoint",
        Destination::Public => "public-endpoint",
    };
    match case.destination {
        Destination::List => {
            let endpoints = [(
                endpoint(id),
                ListEndpoint {
                    payload: PaymentEndpointPayload::new(payload).unwrap(),
                    single_use: false,
                },
            )]
            .into_iter()
            .collect();
            w.list_to_alice_with(&mut shop, &[wallet.app_id()], list_version(1, 1), endpoints)
                .await;
            wallet.receive(None).await.unwrap();
        }
        Destination::Public => {
            super::public::publish(
                &w,
                serde_json::json!({ "public_endpoints": { id: payload } }),
                Some(w.clock.now_ms() + 24 * HOUR_MS),
            );
        }
        _ => {}
    }
    // A fractional signed instant distinguishes the rate's whole-second start.
    w.clock.set_ms(w.clock.now_ms() / 1000 * 1000 + 10_999);
    let expires = w.clock.now_ms() + HOUR_MS;
    let terms = PeriodDetails {
        billing_period: period,
        payment_endpoints: case.details_endpoints.then(|| {
            [(
                endpoint(id),
                PaymentEndpointPayload::new("details-endpoint").unwrap(),
            )]
            .into_iter()
            .collect()
        }),
        rates: (case.cross_asset || !case.details_endpoints).then(|| rates("2.005")),
        expires_at: expires,
    };
    shop.send_period_details(&sale, terms.clone())
        .await
        .unwrap();
    let period_key = shop.payee_activity().await.unwrap()[0].periods[0].record;
    let held = shop
        .app()
        .shared_record(&period_key)
        .await
        .unwrap()
        .unwrap();
    let record = ActivityRecord::read(
        held.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payee,
    )
    .unwrap();
    let named = record.period_details.unwrap();
    let Body::PaymentPeriodDetails(named_body) = named.body() else {
        panic!("period details")
    };
    assert_eq!(named_body, &terms);
    let signed = named.message().created_at_ms;
    let start = paykit_lib::periods::rates_start(signed);
    assert!(signed > start, "the fixture signs at a fractional second");
    if case.newer_details {
        w.clock.advance_ms(1);
        let mut later = terms;
        later.rates = Some(rates("9"));
        later.expires_at += HOUR_MS;
        shop.send_period_details(&sale, later).await.unwrap();
    }
    let paid_at = match case.paid_at {
        PaidAt::BeforeStart => start - 1,
        PaidAt::Start => start,
        PaidAt::BeforeSigned => signed - 1,
        PaidAt::Inside => signed + 1,
        PaidAt::BeforeExpiry => expires - 1,
        PaidAt::Expiry => expires,
        PaidAt::AfterExpiry => expires + 1,
    };
    w.clock.set_ms(paid_at);
    let attempt_id = AttemptId::from_bytes([63; 16]);
    let amount = if case.cross_asset {
        usd(if case.wrong_amount { "2.00" } else { "2.01" })
    } else {
        btc("1")
    };
    let outcome = w
        .rail
        .pay_outside(
            endpoint(id),
            PaymentEndpointPayload::new(payload).unwrap(),
            amount,
            attempt_id,
        )
        .await
        .unwrap();
    let RailOutcome::Paid(paid) = outcome else {
        panic!("payment")
    };
    assert_eq!(paid.paid_at_ms, Some(paid_at));
    // Every proof arrives after expiry. Only independently verified payment time counts.
    w.clock.set_ms(expires + 2 * HOUR_MS);
    let proof = PaymentProof {
        attempt_id,
        billing_period: Some(period),
        payment_endpoint_identifier: endpoint(id),
        payment_app_id: shop.app_id(),
        list_version: matches!(case.destination, Destination::List).then_some(list_version(1, 1)),
        amount: None,
        allowance_id: None,
        label: None,
        payment_reference: None,
        proof: paid.proof,
    }
    .to_body(2)
    .unwrap();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, named.hash()],
        proof,
    );
    shop.receive(None).await.unwrap();
    if case.restored {
        let backup = shop.backup().await.unwrap();
        drop(shop);
        shop = Paykit::restore(
            w.env(&w.bob, store()),
            PaykitConfig::default(),
            &backup,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        .unwrap();
        shop.receive(None).await.unwrap();
    }
    // Evidence the method verified at a time the period doesn't allow is flagged, never
    // credited; one this method can't verify is simply not credited.
    let flagged_or = |result: paykit_sdk::Result<Option<u64>>| match result {
        Err(error) => {
            assert_eq!(
                error.refusal(),
                Some(&paykit_sdk::Refusal::PaymentTimeOutside {
                    paid_at_ms: paid_at
                }),
                "{case:?}"
            );
            None
        }
        Ok(verified) => verified,
    };
    let verified = flagged_or(shop.verify_payment(&period_key).await);
    let receipt = match shop.confirm_payment(&period_key).await {
        Err(error) => {
            assert!(
                matches!(
                    error.refusal(),
                    Some(paykit_sdk::Refusal::PaymentTimeOutside { .. })
                ),
                "{case:?}"
            );
            None
        }
        Ok(receipt) => receipt,
    };
    let view = shop.payee_activity().await.unwrap();
    let held = shop
        .app()
        .shared_record(&period_key)
        .await
        .unwrap()
        .unwrap();
    let record = ActivityRecord::read(
        held.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payee,
    )
    .unwrap();
    assert!(record.proof.is_some(), "late evidence remains stored");
    assert_eq!(verified, expected.then_some(paid_at), "{case:?}");
    assert_eq!(
        (
            receipt.is_some(),
            record.receipt.is_some(),
            view[0].periods[0].credited
        ),
        (expected, expected, expected),
        "{case:?}"
    );
    let checks = w.rail.proof_checks();
    let uses_details = case.per_period_endpoints && case.later_period;
    assert_eq!(
        checks.len(),
        if uses_details && !case.details_endpoints {
            0
        } else {
            2
        },
        "{case:?}"
    );
    for check in checks {
        assert_eq!(
            check.amount,
            if case.cross_asset {
                usd("2.01")
            } else {
                btc("1")
            }
        );
        assert_eq!(
            check.payload.as_ref().map(|p| p.as_str()),
            if uses_details {
                Some("details-endpoint")
            } else if matches!(
                case.destination,
                Destination::Details | Destination::Request
            ) {
                Some(if case.cross_asset {
                    "request-usd"
                } else {
                    "request-btc"
                })
            } else {
                None
            }
        );
    }
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_period_credit_uses_named_rates_for_every_destination() {
    for destination in [
        Destination::Details,
        Destination::Request,
        Destination::List,
        Destination::Public,
    ] {
        check_credit(Case::priced(destination), true).await;
    }
}

#[tokio::test]
async fn test_period_credit_refuses_an_amount_other_than_the_named_rate() {
    for destination in [
        Destination::Details,
        Destination::Request,
        Destination::List,
        Destination::Public,
    ] {
        check_credit(
            Case {
                wrong_amount: true,
                ..Case::priced(destination)
            },
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn test_period_credit_rates_use_the_inclusive_verified_time_interval() {
    for (paid_at, expected) in [
        (PaidAt::BeforeStart, false),
        (PaidAt::Start, true),
        (PaidAt::BeforeSigned, true),
        (PaidAt::Expiry, true),
        (PaidAt::AfterExpiry, false),
    ] {
        check_credit(
            Case {
                paid_at,
                ..Case::priced(Destination::Request)
            },
            expected,
        )
        .await;
    }
}

#[tokio::test]
async fn test_period_credit_same_asset_details_destination_must_be_unexpired() {
    for (paid_at, expected) in [
        (PaidAt::BeforeExpiry, true),
        (PaidAt::Expiry, false),
        (PaidAt::AfterExpiry, false),
    ] {
        check_credit(
            Case {
                paid_at,
                cross_asset: false,
                ..Case::priced(Destination::Details)
            },
            expected,
        )
        .await;
    }
}

#[tokio::test]
async fn test_period_credit_uses_the_proofs_earlier_details_after_restore() {
    check_credit(
        Case {
            newer_details: true,
            restored: true,
            ..Case::priced(Destination::Request)
        },
        true,
    )
    .await;
}

#[tokio::test]
async fn test_period_credit_same_asset_request_ignores_unused_rate_expiry() {
    check_credit(
        Case {
            paid_at: PaidAt::AfterExpiry,
            cross_asset: false,
            ..Case::priced(Destination::Request)
        },
        true,
    )
    .await;
}

async fn check_destination_policy(per_period_endpoints: bool, later_period: bool) {
    for cross_asset in [true, false] {
        for destination in [Destination::Details, Destination::Request] {
            let uses_details = per_period_endpoints && later_period;
            let expected = matches!(destination, Destination::Details) == uses_details;
            check_credit(
                Case {
                    per_period_endpoints,
                    later_period,
                    details_endpoints: true,
                    cross_asset,
                    ..Case::priced(destination)
                },
                expected,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn test_period_credit_fixed_destination_for_first_period() {
    check_destination_policy(false, false).await;
}

#[tokio::test]
async fn test_period_credit_fixed_destination_for_later_period() {
    check_destination_policy(false, true).await;
}

#[tokio::test]
async fn test_period_credit_per_period_destination_for_first_period() {
    check_destination_policy(true, false).await;
}

#[tokio::test]
async fn test_period_credit_per_period_destination_for_later_period() {
    check_destination_policy(true, true).await;
}

#[tokio::test]
async fn test_period_credit_later_period_requires_details_endpoints() {
    for cross_asset in [false, true] {
        check_credit(
            Case {
                per_period_endpoints: true,
                later_period: true,
                cross_asset,
                ..Case::priced(Destination::Request)
            },
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn test_period_credit_request_destination_ignores_unused_endpoint_expiry() {
    for (per_period_endpoints, later_period) in [(false, false), (false, true), (true, false)] {
        check_credit(
            Case {
                per_period_endpoints,
                later_period,
                details_endpoints: true,
                cross_asset: false,
                paid_at: PaidAt::AfterExpiry,
                ..Case::priced(Destination::Request)
            },
            true,
        )
        .await;
    }
}
