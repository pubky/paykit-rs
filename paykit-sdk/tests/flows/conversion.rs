//! Requests priced by conversion rates (Paykit spec 7.2 step 1, 8.2): the amount to each
//! endpoint computed before paying, a payment a rate prices made only by hand at the amount
//! its user confirmed, and the payee crediting that amount and no other.

use paykit_lib::bodies::{Conversion, RequestDestination};
use paykit_lib::{Asset, Decimal, PaymentAmount, PaymentEndpointPayload, PaymentReference, Rate};
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    PayOptions, PayOutcome, Paykit, PricedPayment, RateSource, Refusal, RequestDraft, RequestRef,
};
use pubky_switch_client::clock::Clock;

use crate::common::{btc, endpoint, list_version, request_from, World, HOUR_MS};

const BOLT11: &str = "btc-lightning-bolt11";

fn usd(value: &str) -> PaymentAmount {
    PaymentAmount {
        value: Decimal::new(value).unwrap(),
        asset: Asset::new("usd").unwrap(),
    }
}

/// A one-time request for 10 usd to `endpoints`, with fixed rates `rates`, expiring a day
/// from `now`.
pub(crate) fn priced(now: u64, endpoints: &[(&str, &str)], rates: &[(&str, &str)]) -> RequestDraft {
    let endpoints = endpoints
        .iter()
        .map(|(id, payload)| (endpoint(id), PaymentEndpointPayload::new(payload).unwrap()))
        .collect();
    let rates = rates
        .iter()
        .map(|(asset, value)| {
            Rate::new(Asset::new(asset).unwrap(), Decimal::new(value).unwrap()).unwrap()
        })
        .collect();
    RequestDraft::new(
        usd("10"),
        PaymentReference::new("order-usd").unwrap(),
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(now + 24 * HOUR_MS)
    .with_conversion(Conversion::Fixed(rates))
}

/// The request for 10 usd to one invoice, priced in btc at 0.0000123456 per usd: 0.000123456
/// btc, 0.00012346 rounded up to satoshis.
async fn priced_invoice(w: &World, payload: &str) -> (Paykit, Paykit, RequestRef) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let draft = priced(
        w.clock.now_ms(),
        &[(BOLT11, payload)],
        &[("btc", "0.0000123456")],
    );
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    (wallet, shop, request)
}

/// The shown of a refusal of a payment priced by a rate.
pub(crate) fn priced_of(error: &paykit_sdk::PaykitError) -> PricedPayment {
    match error.refusal() {
        Some(Refusal::PricedByRate { payment }) => (**payment).clone(),
        other => panic!("{other:?} {error:?}"),
    }
}

/// Nothing written: no journal entry, nothing the rail was asked to pay.
async fn nothing_paid(w: &World, wallet: &Paykit) {
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_conversion_priced_payment_refused_without_confirmation() {
    let w = World::new();
    let (mut wallet, _shop, request) = priced_invoice(&w, "lnbc1invoice").await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let shown = priced_of(&refused);
    assert_eq!(shown.request, request);
    assert_eq!(shown.endpoint_id, endpoint(BOLT11));
    assert_eq!(shown.requested, usd("10"));
    assert_eq!(shown.source, RateSource::Request);
    assert_eq!(shown.rate.value().as_str(), "0.0000123456");
    assert_eq!(shown.amount, btc("0.00012346"));
    nothing_paid(&w, &wallet).await;
}

#[tokio::test]
async fn test_conversion_priced_payment_pays_the_confirmed_amount() {
    let w = World::new();
    let (mut wallet, _shop, request) = priced_invoice(&w, "lnbc1invoice").await;
    let shown = priced_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    let outcome = wallet
        .pay_request(&request, PayOptions::default().with_rate_authorized(shown))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    // The rail was asked for exactly the amount computed, in the endpoint's asset.
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(btc("0.00012346")));
    assert_eq!(w.rail.executions(&attempt_id), 1);
    let payments = wallet.payments().await.unwrap();
    assert_eq!(payments[0].amount, usd("10"));
    assert_eq!(payments[0].paid, Some(btc("0.00012346")));
    assert_eq!(
        payments[0].rate.as_ref().map(|r| r.value().as_str()),
        Some("0.0000123456")
    );
}

#[tokio::test]
async fn test_conversion_invoice_of_the_computed_amount_passes_both_transfer_checks() {
    // An invoice fixing the computed amount transfers exactly that: the transfer check at
    // planning and again just before the call holds the method to the amount computed, not
    // the request's.
    let w = World::new();
    let (mut wallet, _shop, request) = priced_invoice(&w, "lnbc1?amount=0.00012346").await;
    let shown = priced_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    let outcome = wallet
        .pay_request(&request, PayOptions::default().with_rate_authorized(shown))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(btc("0.00012346")));
}

#[tokio::test]
async fn test_conversion_changed_price_refused_again() {
    let w = World::new();
    let (mut wallet, _shop, request) = priced_invoice(&w, "lnbc1invoice").await;
    let shown = priced_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    // A confirmation of another amount is no confirmation of this one.
    let mut other = shown.clone();
    other.amount = btc("0.0002");
    let refused = wallet
        .pay_request(&request, PayOptions::default().with_rate_authorized(other))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(priced_of(&refused), shown);
    // The method names another smallest unit since: the amount computed now differs, and
    // the shown confirmed before is refused again with the new one.
    w.rail.set_fraction_digits(endpoint(BOLT11), Some(6));
    let refused = wallet
        .pay_request(&request, PayOptions::default().with_rate_authorized(shown))
        .await
        .outcome
        .unwrap_err();
    let new = priced_of(&refused);
    assert_eq!(new.amount, btc("0.000124"));
    nothing_paid(&w, &wallet).await;
    let PayOutcome::Paid { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default().with_rate_authorized(new))
        .await
        .outcome
        .unwrap()
    else {
        panic!("paid");
    };
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(btc("0.000124")));
}

#[tokio::test]
async fn test_conversion_endpoint_of_another_asset_no_rate_prices_unusable() {
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![endpoint("eur-sepa-iban"), endpoint(BOLT11)]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let draft = priced(
        w.clock.now_ms(),
        &[("eur-sepa-iban", "DE00iban"), (BOLT11, "lnbc1invoice")],
        &[("btc", "0.0000123456")],
    );
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_endpoint(endpoint("eur-sepa-iban")),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    nothing_paid(&w, &wallet).await;
    // The method's first choice unusable, the next one, a rate prices, is what is shown.
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert_eq!(priced_of(&refused).endpoint_id, endpoint(BOLT11));
}

#[tokio::test]
async fn test_conversion_endpoint_of_the_requested_asset_paid_its_amount() {
    // No rate prices an endpoint of the requested asset: it is paid the request's amount,
    // with no confirmation of a rate.
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![endpoint("usd-ach-account"), endpoint(BOLT11)]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let draft = priced(
        w.clock.now_ms(),
        &[("usd-ach-account", "acct-1"), (BOLT11, "lnbc1invoice")],
        &[("btc", "0.0000123456")],
    );
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let PayOutcome::Paid { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("paid");
    };
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(usd("10")));
    let payments = wallet.payments().await.unwrap();
    assert_eq!(
        (&payments[0].paid, &payments[0].rate),
        (&Some(usd("10")), &None)
    );
}

#[tokio::test]
async fn test_conversion_automatic_payment_of_a_priced_request_refused() {
    let w = World::new();
    let (mut wallet, _shop, request) = priced_invoice(&w, "lnbc1invoice").await;
    let shown = priced_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    // Even with the amount confirmed, an automatic payment pays nothing a rate prices.
    let refused = wallet
        .pay_request_automatically(
            &request,
            PayOptions::default().with_rate_authorized(shown.clone()),
        )
        .await
        .outcome
        .unwrap_err();
    assert_eq!(priced_of(&refused), shown);
    nothing_paid(&w, &wallet).await;
}

#[tokio::test]
async fn test_conversion_rail_without_the_endpoints_unit_refuses() {
    let w = World::new();
    w.rail.set_fraction_digits(endpoint(BOLT11), None);
    let (mut wallet, _shop, request) = priced_invoice(&w, "lnbc1invoice").await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    nothing_paid(&w, &wallet).await;
    // Once the method names the unit, the amount is shown.
    w.rail.set_fraction_digits(endpoint(BOLT11), Some(11));
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert_eq!(priced_of(&refused).amount, btc("0.000123456"));
}

#[tokio::test]
async fn test_conversion_payee_credits_a_priced_payment_at_the_confirmed_amount() {
    let w = World::new();
    let (mut wallet, mut shop, request) = priced_invoice(&w, "lnbc1invoice").await;
    let shown = priced_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    wallet
        .pay_request(&request, PayOptions::default().with_rate_authorized(shown))
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert!(shop
        .verify_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    assert!(shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_conversion_payee_credits_no_priced_payment_of_another_amount() {
    // A counterparty paid another amount outside this SDK. The signed request's rate
    // still determines the amount its proof must establish.
    let w = World::new();
    let (_wallet, mut shop, request) = priced_invoice(&w, "lnbc1?amount=0.0003").await;
    let attempt_id = paykit_lib::AttemptId::from_bytes([31; 16]);
    let outcome = w
        .rail
        .pay_outside(
            endpoint(BOLT11),
            PaymentEndpointPayload::new("lnbc1?amount=0.0003").unwrap(),
            btc("0.0003"),
            attempt_id,
        )
        .await
        .unwrap();
    let paykit_sdk::rail::RailOutcome::Paid(paid) = outcome else {
        panic!("{outcome:?}")
    };
    let body = paykit_lib::bodies::PaymentProof {
        attempt_id,
        billing_period: None,
        payment_endpoint_identifier: endpoint(BOLT11),
        payment_app_id: shop.app_id(),
        list_version: None,
        amount: None,
        allowance_id: None,
        label: None,
        payment_reference: None,
        proof: paid.proof,
    }
    .to_body(1)
    .unwrap();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );

    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(shop.verify_payment(&sales[0].record).await.unwrap(), None);
    assert_eq!(shop.confirm_payment(&sales[0].record).await.unwrap(), None);
}

/// The world's rail declared idempotent, so a resume repeats a dispatch left in flight.
fn idempotent(w: &World) {
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true),
    );
}

/// Alice's wallet, which keeps its state in `store` and pays with a day's orphan window, with
/// bob's request for 10 usd to one invoice priced in btc, paid by hand and left in flight.
async fn priced_in_flight(
    w: &World,
    store: std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
) -> (Paykit, paykit_lib::AttemptId) {
    idempotent(w);
    let mut wallet = w
        .wallet_with("wallet", store, crate::common::day_window())
        .await;
    let mut shop = w.shop().await;
    let draft = priced(
        w.clock.now_ms(),
        &[(BOLT11, "lnbc1invoice")],
        &[("btc", "0.0000123456")],
    );
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let shown = priced_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default().with_rate_authorized(shown))
        .await
        .outcome
        .unwrap()
    else {
        panic!("in flight");
    };
    (wallet, attempt_id)
}

/// `wallet`'s one journal entry with `edit` applied, written back to its store.
async fn edit_entry(wallet: &mut Paykit, edit: impl FnOnce(&mut serde_json::Value)) {
    let held = wallet.app().protocol_scan("paykit", b"j/").await.unwrap();
    assert_eq!(held.len(), 1);
    let mut value: serde_json::Value = serde_json::from_slice(&held[0].1).unwrap();
    edit(&mut value["d"]);
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: held[0].0.clone(),
                value: serde_json::to_vec(&value).unwrap(),
            }],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_conversion_priced_payment_in_flight_repeats_at_the_confirmed_amount() {
    // A priced payment left in flight is repeated at a resume, the method idempotent on the
    // attempt: the repeat asks the method for the amount confirmed, never the request's,
    // and money moves once.
    let w = World::new();
    let (mut wallet, attempt_id) = priced_in_flight(&w, crate::common::store()).await;
    w.clock.advance_ms(60_000);
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        w.rail.pay_call_amounts(&attempt_id),
        vec![btc("0.00012346"), btc("0.00012346")]
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_conversion_tampered_amount_in_flight_never_paid() {
    // The store's entry of a priced payment in flight names another amount, in the same
    // asset, which its checks alone can't tell from one confirmed: the final check computes
    // the amount again by the kept rate, and the repeat is never made.
    let w = World::new();
    let (mut wallet, attempt_id) = priced_in_flight(&w, crate::common::store()).await;
    edit_entry(&mut wallet, |entry| {
        entry["payment_amount"] = serde_json::json!({ "value": "0.001", "asset": "btc" });
    })
    .await;
    w.clock.advance_ms(60_000);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        resumed
            .iter()
            .any(|o| matches!(o, PayOutcome::Uncertain { attempt_id: a, .. } if *a == attempt_id)),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_conversion_live_entry_with_another_rate_or_source_refused_at_read() {
    // Every read of the journal binds a price to the request it keeps: a rate the request's
    // fixed rates don't give the endpoint, or period details' rates on a one-time request,
    // is a corrupt store, and the resume pays nothing.
    for (what, member, value) in [
        ("another rate", "rate_value", serde_json::json!("0.00002")),
        (
            "period details' rates",
            "source",
            serde_json::json!({ "kind": "period_details", "msg_id": "details" }),
        ),
    ] {
        let w = World::new();
        let (mut wallet, attempt_id) = priced_in_flight(&w, crate::common::store()).await;
        edit_entry(&mut wallet, |entry| entry["price"][member] = value).await;
        w.clock.advance_ms(60_000);
        let error = wallet.resume_payments().await.unwrap_err();
        assert!(crate::common::is_corrupt(&error), "{what}: {error:?}");
        assert!(wallet.payments().await.is_err(), "{what}");
        assert_eq!(w.rail.pay_calls(&attempt_id), 1, "{what}");
    }
}

#[tokio::test]
async fn test_conversion_amount_finer_than_the_unit_paid_rounded_up_and_credited() {
    // A request for less than a satoshi, without conversion terms, to an endpoint of its
    // asset: paid the amount rounded up once to the method's unit, and the payee, by the
    // same rule with its own method's unit, credits it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("0.000000001", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let PayOutcome::Paid { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("paid");
    };
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(btc("0.00000001")));
    let payments = wallet.payments().await.unwrap();
    assert_eq!(payments[0].amount, btc("0.000000001"));
    assert_eq!(payments[0].paid, Some(btc("0.00000001")));
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert!(shop
        .verify_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    assert!(shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_conversion_endpoint_whose_unit_the_method_cant_name_unusable() {
    // The method names no unit for the endpoint: the amount to pay can't be computed, so
    // the endpoint is unusable, nothing written or paid.
    let w = World::new();
    w.rail.set_fraction_digits(endpoint(BOLT11), None);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("0.000000001", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
}

/// Bob's shop offers Alice a reusable Lightning endpoint that expires in 30 days. His other
/// wallet offers a reusable on-chain endpoint that can expire in an hour. Returns the shop.
async fn two_lists(w: &World, wallet: &mut Paykit, seller_wallet_expires: bool) -> Paykit {
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut shop = w.shop().await;
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    let one = |id: &str, payload: &str| {
        let mut endpoints = std::collections::BTreeMap::new();
        endpoints.insert(
            endpoint(id),
            paykit_lib::bodies::ListEndpoint {
                payload: PaymentEndpointPayload::new(payload).unwrap(),
                single_use: false,
            },
        );
        endpoints
    };
    let now = w.clock.now_ms();
    let expires_at = if seller_wallet_expires {
        now + HOUR_MS
    } else {
        now + 30 * 24 * HOUR_MS
    };
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: list_version(1, 1),
        payment_endpoints: one("btc-bitcoin-p2wpkh", "bc1qwallet"),
        expires_at,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "seller-wallet",
        &w.alice,
        &[wallet.app_id()],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    w.list_to_alice_with(
        &mut shop,
        &[wallet.app_id()],
        list_version(1, 2),
        one("btc-lightning-bolt12", "lno1shop"),
    )
    .await;
    wallet.receive(None).await.unwrap();
    shop
}

/// How many requests for a Private Payment List `app` sent `to`.
async fn list_asks(app: &Paykit, to: &[u8; 32]) -> usize {
    let sent = app.app().sent_to(to, 0, u64::MAX).await.unwrap();
    sent.iter()
        .filter(|m| m.kind == "paykit.private_payment_list_request")
        .count()
}

/// Bob's shop's request for 10 usd to any of his lists, priced only in eth: every btc
/// endpoint is unusable under its terms.
async fn eth_priced_request(w: &World, shop: &mut Paykit, wallet: &mut Paykit) -> RequestRef {
    let rate = Rate::new(Asset::new("eth").unwrap(), Decimal::new("0.004").unwrap()).unwrap();
    let draft = RequestDraft::new(
        usd("10"),
        PaymentReference::new("order-usd").unwrap(),
        RequestDestination::Any,
    )
    .with_proposal_expires_at_ms(w.clock.now_ms() + 24 * HOUR_MS)
    .with_conversion(Conversion::Fixed(vec![rate]));
    let request = request_from(shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    request
}

#[tokio::test]
async fn test_conversion_unusable_list_endpoints_beside_an_expired_list_ask_for_it() {
    // The live list's endpoint is unusable under the terms, but the seller wallet's list has
    // expired, and its current one may serve: the seller wallet is asked, and alice waits for it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = two_lists(&w, &mut wallet, true).await;
    let request = eth_priced_request(&w, &mut shop, &mut wallet).await;
    w.clock.advance_ms(2 * HOUR_MS);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    // Each app whose list is held is asked for its current one.
    assert_eq!(list_asks(&wallet, &w.bob_id()).await, 2);
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_conversion_unusable_list_endpoints_beside_a_used_up_list_ask_for_it() {
    // The shop's list is used up: alice paid its single-use endpoint. The seller wallet's live list
    // is unusable under the terms, but a newer list from the shop may serve: alice is told
    // she waits for a list, and bob is asked (Paykit spec 8.4).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(
            paykit_sdk::UnrequestedPayment::new(w.bob_id(), btc("0.0015"))
                .with_options(PayOptions::default().with_endpoint(endpoint("btc-bitcoin-p2wpkh"))),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt12"),
        paykit_lib::bodies::ListEndpoint {
            payload: PaymentEndpointPayload::new("lno1wallet").unwrap(),
            single_use: false,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: list_version(1, 1),
        payment_endpoints: endpoints,
        expires_at: w.clock.now_ms() + 30 * 24 * HOUR_MS,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "seller-wallet",
        &w.alice,
        &[wallet.app_id()],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    wallet.receive(None).await.unwrap();
    let asked = list_asks(&wallet, &w.bob_id()).await;
    let request = eth_priced_request(&w, &mut shop, &mut wallet).await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert!(list_asks(&wallet, &w.bob_id()).await > asked);
    assert_eq!(wallet.payments().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_conversion_unusable_list_endpoints_refused_without_a_list_to_ask() {
    // Both lists live, every endpoint unusable under the terms: a newer list needn't
    // differ, so the payment is refused for the terms and the payee isn't asked.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = two_lists(&w, &mut wallet, false).await;
    let request = eth_priced_request(&w, &mut shop, &mut wallet).await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    assert_eq!(list_asks(&wallet, &w.bob_id()).await, 0);
}

#[tokio::test]
async fn test_conversion_transfer_confirmation_cannot_change_a_priced_amount() {
    let w = World::new();
    let (mut wallet, _shop, request) = priced_invoice(&w, "lnbc1?amount=0.0003").await;
    let shown = priced_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    let options = PayOptions::default().with_rate_authorized(shown);
    let refused = wallet
        .pay_request(&request, options.clone())
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::Transfer { quote, .. }) = refused.refusal() else {
        panic!("{refused:?}")
    };
    let refused = wallet
        .pay_request(&request, options.with_transfer_confirmed((**quote).clone()))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Transfer { .. })),
        "{refused:?}"
    );
    nothing_paid(&w, &wallet).await;
}

#[tokio::test]
async fn test_conversion_fixed_terms_require_the_kept_price() {
    let w = World::new();
    let (mut wallet, attempt_id) = priced_in_flight(&w, crate::common::store()).await;
    edit_entry(&mut wallet, |entry| {
        entry["price"] = serde_json::Value::Null;
        entry["payment_amount"] = entry["amount"].clone();
    })
    .await;
    let refused = wallet.resume_payments().await.unwrap_err();
    assert!(crate::common::is_corrupt(&refused), "{refused:?}");
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_conversion_blocked_confirmation_uses_the_rounded_amount() {
    let w = World::new();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new()
                .pay()
                .manage_contacts()
                .build()
                .unwrap(),
            paykit_sdk::PaykitConfig::default(),
            crate::common::store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("0.000000001", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    wallet.block(&w.bob_id()).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::Blocked { payment }) = refused.refusal() else {
        panic!("{refused:?}")
    };
    assert_eq!(payment.amount, btc("0.00000001"));
    assert_eq!(payment.period, None);
    let mut nominal = (**payment).clone();
    nominal.amount = btc("0.000000001");
    assert!(matches!(
        wallet
            .pay_request(
                &request,
                PayOptions::default().with_blocked_confirmed(nominal)
            )
            .await
            .outcome
            .unwrap_err()
            .refusal(),
        Some(Refusal::Blocked { .. })
    ));
    let outcome = wallet
        .pay_request(
            &request,
            PayOptions::default().with_blocked_confirmed((**payment).clone()),
        )
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(btc("0.00000001")));
}

#[tokio::test]
async fn test_conversion_dispatchable_payment_requires_its_transfer_bound() {
    let w = World::new();
    let (mut wallet, attempt_id) = priced_in_flight(&w, crate::common::store()).await;
    edit_entry(&mut wallet, |entry| {
        entry["price"] = serde_json::Value::Null;
        entry["transfer_bound"] = serde_json::Value::Null;
        entry["payment_amount"] = entry["amount"].clone();
        entry["restored"] = serde_json::json!(true);
    })
    .await;
    let refused = wallet.resume_payments().await.unwrap_err();
    assert!(crate::common::is_corrupt(&refused), "{refused:?}");
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

async fn manual_invoice(w: &World, payload: &str) -> (Paykit, RequestRef) {
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new()
                .pay()
                .manage_contacts()
                .build()
                .unwrap(),
            crate::common::day_window(),
            crate::common::store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let draft = RequestDraft::new(
        btc("1"),
        PaymentReference::new("manual-invoice").unwrap(),
        RequestDestination::Endpoints(
            [(
                endpoint(BOLT11),
                PaymentEndpointPayload::new(payload).unwrap(),
            )]
            .into(),
        ),
    )
    .with_proposal_expires_at_ms(w.clock.now_ms() + 24 * HOUR_MS);
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    (wallet, request)
}

fn transfer_of(error: &paykit_sdk::PaykitError) -> paykit_sdk::TransferQuote {
    let Some(Refusal::Transfer { quote, .. }) = error.refusal() else {
        panic!("{error:?}")
    };
    (**quote).clone()
}

#[tokio::test]
async fn test_conversion_manual_override_binds_the_actual_blocked_amount() {
    let w = World::new();
    let (mut wallet, request) = manual_invoice(&w, "lnbc1?amount=3").await;
    let transfer = transfer_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    let options = PayOptions::default().with_transfer_confirmed(transfer);
    wallet.block(&w.bob_id()).await.unwrap();
    let refused = wallet
        .pay_request(&request, options.clone())
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::Blocked { payment }) = refused.refusal() else {
        panic!("{refused:?}")
    };
    assert_eq!(payment.amount, btc("3"));
    let mut nominal = (**payment).clone();
    nominal.amount = btc("1");
    assert!(matches!(
        wallet
            .pay_request(&request, options.clone().with_blocked_confirmed(nominal))
            .await
            .outcome
            .unwrap_err()
            .refusal(),
        Some(Refusal::Blocked { .. })
    ));
    let outcome = wallet
        .pay_request(
            &request,
            options.with_blocked_confirmed((**payment).clone()),
        )
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}")
    };
    let view = wallet.payments().await.unwrap().remove(0);
    assert_eq!(view.amount, btc("1"));
    assert_eq!(view.paid, Some(btc("3")));
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(btc("3")));
    assert_eq!(w.rail.pay_call_amounts(&attempt_id), vec![btc("1")]);
}

#[tokio::test]
async fn test_conversion_manual_unknown_transfer_keeps_its_amount_unknown() {
    let w = World::new();
    let (mut wallet, request) = manual_invoice(&w, "lnbc1?amount=unknown").await;
    let transfer = transfer_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    assert_eq!(transfer.transfer.amount, None);
    let outcome = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(transfer),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(wallet.payments().await.unwrap()[0].paid, None);
}

#[tokio::test]
async fn test_conversion_unknown_transfer_cannot_supply_a_blocked_amount() {
    let w = World::new();
    let (mut wallet, request) = manual_invoice(&w, "lnbc1?amount=unknown").await;
    let transfer = transfer_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    wallet.block(&w.bob_id()).await.unwrap();
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(transfer),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(reason)) if reason.contains("transfer amount is unknown")),
        "{refused:?}"
    );
    nothing_paid(&w, &wallet).await;
}

#[tokio::test]
async fn test_conversion_automatic_payment_ignores_a_manual_transfer_override() {
    let w = World::new();
    let (mut wallet, request) = manual_invoice(&w, "lnbc1?amount=3").await;
    let transfer = transfer_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    let refused = wallet
        .pay_request_automatically(
            &request,
            PayOptions::default().with_transfer_confirmed(transfer),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Transfer { .. })),
        "{refused:?}"
    );
    nothing_paid(&w, &wallet).await;
}

/// Alice's wallet, with bob's request for 10 usd without conversion terms to one endpoint,
/// `endpoint_id`, which the payment method pays and reports transferring exactly the amount
/// handed to it: the requested amount and asset, as a method that converts by itself would.
async fn unconverted_request(
    w: &World,
    wallet: &mut Paykit,
    endpoint_id: &str,
    payload: &str,
) -> (Paykit, RequestRef) {
    w.rail.set_endpoint_identifiers(vec![endpoint(endpoint_id)]);
    let mut shop = w.shop().await;
    let draft = RequestDraft::new(
        usd("10"),
        PaymentReference::new("order-unconverted").unwrap(),
        RequestDestination::Endpoints(
            [(
                endpoint(endpoint_id),
                PaymentEndpointPayload::new(payload).unwrap(),
            )]
            .into(),
        ),
    )
    .with_proposal_expires_at_ms(w.clock.now_ms() + 24 * HOUR_MS);
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    (shop, request)
}

#[tokio::test]
async fn test_conversion_automatic_cross_asset_payment_without_terms_refused() {
    // A request without conversion terms promises no rate: an automatic payment never pays
    // it to an endpoint of another asset, though the method reports the requested amount.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let (_shop, request) = unconverted_request(&w, &mut wallet, "eur-sepa-iban", "DE00iban").await;
    let refused = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(why)) if why.contains("only by hand")),
        "{refused:?}"
    );
    nothing_paid(&w, &wallet).await;
}

#[tokio::test]
async fn test_conversion_cross_asset_payment_without_terms_needs_the_confirmed_quote() {
    // By hand, the payment is refused with what the method would transfer, though that is
    // the requested amount and asset, and is paid once its user confirms exactly that.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let (mut shop, request) =
        unconverted_request(&w, &mut wallet, "eur-sepa-iban", "DE00iban").await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::Transfer { reason, quote }) = refused.refusal() else {
        panic!("{refused:?}")
    };
    assert!(reason.contains("only by hand"), "{reason}");
    assert_eq!(quote.transfer.amount, Some(usd("10")));
    nothing_paid(&w, &wallet).await;
    // Automatic, the confirmation is ignored.
    let refused = wallet
        .pay_request_automatically(
            &request,
            PayOptions::default().with_transfer_confirmed((**quote).clone()),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    nothing_paid(&w, &wallet).await;
    let outcome = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed((**quote).clone()),
        )
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(usd("10")));
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    let [sale] = sales.as_slice() else {
        panic!("expected the paid request");
    };
    assert!(shop.verify_payment(&sale.record).await.unwrap().is_some());
    assert!(shop.confirm_payment(&sale.record).await.unwrap().is_some());
}

#[tokio::test]
async fn test_conversion_cross_asset_payment_in_flight_repeats_only_as_confirmed() {
    // The first repeat keeps the confirmed transfer. A new amount from the method then
    // leaves the same attempt uncertain.
    let w = World::new();
    idempotent(&w);
    let mut wallet = w
        .wallet_with(
            "wallet",
            crate::common::store(),
            crate::common::day_window(),
        )
        .await;
    let (_shop, request) = unconverted_request(&w, &mut wallet, "eur-sepa-iban", "DE00iban").await;
    let quote = transfer_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err(),
    );
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(quote),
        )
        .await
        .outcome
        .unwrap()
    else {
        panic!("in flight");
    };
    w.clock.advance_ms(60_000);
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_call_amounts(&attempt_id), [usd("10"), usd("10")]);

    w.rail.set_transfer_amount(Some(usd("11")));
    w.clock.advance_ms(60_000);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        resumed.iter().any(|item| matches!(
            item, PayOutcome::Uncertain { attempt_id: id, .. } if *id == attempt_id
        )),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 2);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_conversion_same_asset_payment_without_terms_pays_automatically() {
    // An endpoint of the requested asset needs no confirmation: an automatic payment pays
    // it the requested amount, as before.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let (_shop, request) = unconverted_request(&w, &mut wallet, "usd-ach-account", "acct-1").await;
    let outcome = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(usd("10")));
}
