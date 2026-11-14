//! Recurring requests' subscriptions and Billing Periods (Paykit spec 8.2; Paykit wire 4.4,
//! 4.6, 4.9, 5.3): the payee sends a recurring request, the payer accepts or rejects it,
//! the payee sends period details for a period, and each side keeps its period records.

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_lib::bodies::{PaymentProof, PeriodDetails, RequestDestination};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
use paykit_lib::{AttemptId, PaymentEndpointPayload, PaymentReference};
use paykit_sdk::rail::{Rail, RailOutcome};
use paykit_sdk::{
    AppScopes, PayeeStatus, Paykit, PaykitConfig, PaykitError, Refusal, RequestDraft, RequestRef,
    RequestStatus,
};
use pubky_switch_client::app::{
    AppEvent, Collection as SwitchCollection, OpBatch, RecordContent, RecordTarget, Unopened,
};
use pubky_switch_client::clock::Clock;
use serde_json::json;

use crate::common::{btc, draft, endpoint, store, World, HOUR_MS};

const DAY_MS: u64 = 24 * HOUR_MS;

/// A monthly request for 1 btc, from now, paid to one invoice of the sending app, its
/// proposal open a day; it ends at `ends_at`, when set.
pub(crate) fn monthly(now: u64, ends_at: Option<u64>) -> RequestDraft {
    let mut endpoints = BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt11"),
        PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
    );
    let rule = AnchoredRule::new(now, 1, Unit::Month).unwrap();
    RequestDraft::new(
        btc("1"),
        PaymentReference::new("plan-1").unwrap(),
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(now + DAY_MS)
    .with_recurrence(Recurrence::new(rule, now, ends_at).unwrap())
}

/// The request's first Billing Period: January 2026.
fn january() -> PeriodRef {
    let start = pubky_switch_client::testing::START_MS;
    PeriodRef {
        starts_at: start,
        ends_at: start + 31 * DAY_MS,
    }
}

/// Details for `period`, paid to `invoice`, valid until `expires_at`.
fn details(period: PeriodRef, invoice: &str, expires_at: u64) -> PeriodDetails {
    let mut endpoints = BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt11"),
        PaymentEndpointPayload::new(invoice).unwrap(),
    );
    PeriodDetails {
        billing_period: period,
        payment_endpoints: Some(endpoints),
        rates: None,
        expires_at,
    }
}

/// Details for January, paid to an invoice, valid until `expires_at`.
pub(crate) fn january_details(expires_at: u64) -> PeriodDetails {
    details(january(), "lnbc1january", expires_at)
}

/// Bob's shop sends alice's `wallet` a monthly request. Returns the request.
async fn requested(w: &World, shop: &mut Paykit, wallet: &mut Paykit) -> RequestRef {
    let sent = shop
        .send_request(w.alice_id(), monthly(w.clock.now_ms(), None))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    sent.request.unwrap()
}

/// Alice's wallet accepts bob's monthly request, and the shop reads the acceptance. Returns
/// the request and the shop's record of it.
async fn subscribed(w: &World, shop: &mut Paykit, wallet: &mut Paykit) -> (RequestRef, [u8; 32]) {
    let request = requested(w, shop, wallet).await;
    wallet.accept_subscription(&request).await.unwrap();
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap();
    assert_eq!(sale.len(), 1);
    (request, sale[0].record)
}

fn refusal(error: &PaykitError) -> Refusal {
    error
        .refusal()
        .cloned()
        .unwrap_or_else(|| panic!("{error:?}"))
}

/// Whether `summary` shows a message of `kind` received, its effect applied.
fn received(summary: &paykit_sdk::ReceiveSummary, kind: &str) -> bool {
    summary
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::MessageReceived { kind: k, .. } if k == kind))
}

#[tokio::test]
async fn test_periods_subscription_accepted_then_details_reach_the_payer() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    let view = wallet.request(&request).await.unwrap();
    assert!(view.recurring && view.recurrence.is_some());
    assert_eq!(view.status, RequestStatus::Open);
    wallet.accept_subscription(&request).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Subscribed
    );
    shop.receive(None).await.unwrap();
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert_eq!(sale.status, PayeeStatus::Accepted);
    assert!(sale.recurring);
    let now = w.clock.now_ms();
    let first = shop
        .send_period_details(&sale.record, details(january(), "lnbc1first", now + DAY_MS))
        .await
        .unwrap();
    w.clock.advance_ms(1_000);
    let second = shop
        .send_period_details(
            &sale.record,
            details(january(), "lnbc1second", now + 2 * DAY_MS),
        )
        .await
        .unwrap();
    // The payee's period record holds both, shown with the request, not as a sale of its
    // own.
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].periods.len(), 1);
    assert_eq!(sales[0].periods[0].details.len(), 2);
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
    assert_eq!(view.periods.len(), 1);
    let period = &view.periods[0];
    assert_eq!(period.billing_period, january());
    // Each stays, the latest first: a later one replaces none.
    let ids: Vec<&str> = period.details.iter().map(|d| d.msg_id.as_str()).collect();
    assert_eq!(ids, [second.msg_id.as_str(), first.msg_id.as_str()]);
    assert_eq!(period.details[0].expires_at_ms, now + 2 * DAY_MS);
    assert!(!period.paid);
}

#[tokio::test]
async fn test_periods_details_refused_before_acceptance_and_out_of_the_request() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    let sale = shop.payee_activity().await.unwrap()[0].record;
    let now = w.clock.now_ms();
    let refused = shop
        .send_period_details(&sale, details(january(), "lnbc1p", now + DAY_MS))
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::NotSubscribed);
    wallet.accept_subscription(&request).await.unwrap();
    shop.receive(None).await.unwrap();
    // A period that isn't one of the request's, and details that expired, are refused.
    // Details are valid until their `expires_at`, that millisecond included: these expired
    // the millisecond before.
    let mut off = january();
    off.starts_at += 1;
    let refused = shop
        .send_period_details(&sale, details(off, "lnbc1p", now + DAY_MS))
        .await
        .unwrap_err();
    assert!(matches!(refused, PaykitError::Lib(_)), "{refused:?}");
    let refused = shop
        .send_period_details(&sale, details(january(), "lnbc1p", w.clock.now_ms() - 1))
        .await
        .unwrap_err();
    assert!(matches!(refused, PaykitError::Lib(_)), "{refused:?}");
    // A one-time request has no periods.
    let one_time = shop
        .send_request(w.alice_id(), draft("1", now))
        .await
        .unwrap()
        .request
        .unwrap();
    let held = shop.payee_activity().await.unwrap();
    let one_time = held
        .iter()
        .find(|s| s.request_hash == Some(one_time.request_hash))
        .unwrap()
        .record;
    let refused = shop
        .send_period_details(&one_time, details(january(), "lnbc1p", now + DAY_MS))
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
    // Cancelled by the payee, the subscription takes no more details.
    shop.cancel_sent_request(&sale, None).await.unwrap();
    let refused = shop
        .send_period_details(&sale, details(january(), "lnbc1p", now + DAY_MS))
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Cancelled);
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Cancelled);
    assert!(view.periods.is_empty());
}

#[tokio::test]
async fn test_periods_details_valid_at_their_own_expiry_sent_and_stored() {
    // Details are valid until their `expires_at`, that millisecond included (Paykit wire
    // 4.9): on a verified service time equal to it, they are sent and stored.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let expires_at = w.clock.now_ms();
    let sent = shop
        .send_period_details(&sale, details(january(), "lnbc1p", expires_at))
        .await
        .unwrap();
    assert_eq!(shop.app().highest_service_time(), Some(expires_at));
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert!(
        view.periods
            .iter()
            .any(|p| p.details.iter().any(|d| d.msg_id == sent.msg_id)),
        "{view:?}"
    );
}

#[tokio::test]
async fn test_periods_ninth_details_refused_the_payer_holding_eight() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    for i in 0..8 {
        shop.send_period_details(
            &sale,
            details(january(), &format!("lnbc1p{i}"), now + DAY_MS),
        )
        .await
        .unwrap();
    }
    let refused = shop
        .send_period_details(&sale, details(january(), "lnbc1p9", now + DAY_MS))
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.periods[0].details.len(), 8);
}

#[tokio::test]
async fn test_periods_accept_refused_once_answered_and_for_a_one_time_request() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    // Accepted, the subscription is neither accepted nor rejected again.
    let again = wallet.accept_subscription(&request).await.unwrap_err();
    assert_eq!(refusal(&again), Refusal::Subscribed);
    let rejected = wallet.reject_request(&request, None).await.unwrap_err();
    assert_eq!(refusal(&rejected), Refusal::Subscribed);
    // A rejected one isn't accepted.
    let other = requested(&w, &mut shop, &mut wallet).await;
    wallet.reject_request(&other, None).await.unwrap();
    let refused = wallet.accept_subscription(&other).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Rejected);
    // A one-time request is accepted by paying it.
    let one_time = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    let refused = wallet.accept_subscription(&one_time).await.unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
    // The shop got one acceptance and one rejection.
    let summary = shop.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_request_acceptance"));
    assert!(received(&summary, "paykit.payment_request_rejection"));
}

#[tokio::test]
async fn test_periods_accept_past_the_expiry_refused_by_the_service() {
    // The wallet hasn't heard from its switch since the proposal expired: its batch goes,
    // and the service refuses it, the acceptance's time condition past. Nothing is sent.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    w.clock.advance_ms(DAY_MS + 1);
    let refused = wallet.accept_subscription(&request).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::DeadlinePassed);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Expired
    );
    let summary = shop.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_request_acceptance"));
}

#[tokio::test]
async fn test_periods_accept_without_an_expiry_has_no_time_bound() {
    // A request without a proposal expiry is accepted whenever: its batch carries the
    // record version alone.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut open = monthly(w.clock.now_ms(), None);
    open.proposal_expires_at_ms = None;
    let request = shop
        .send_request(w.alice_id(), open)
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(40 * DAY_MS);
    wallet.app_mut().read_claim(&[0; 32]).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Subscribed
    );
}

#[tokio::test]
async fn test_periods_subscription_ends_after_its_last_period() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    // Two periods: January, and February, whole though the recurrence ends in it.
    let request = shop
        .send_request(w.alice_id(), monthly(now, Some(now + 40 * DAY_MS)))
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    let february_end = now + (31 + 28) * DAY_MS;
    w.clock.advance_ms(february_end - 1 - w.clock.now_ms());
    wallet.app_mut().read_claim(&[0; 32]).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Subscribed
    );
    w.clock.advance_ms(1);
    wallet.app_mut().read_claim(&[0; 32]).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Ended
    );
}

#[tokio::test]
async fn test_periods_draft_with_both_deadlines_refused_period_deadline_sent() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let both = monthly(now, None)
        .with_period_deadline_s(86_400)
        .with_payment_deadline_ms(now + DAY_MS);
    let refused = shop.send_request(w.alice_id(), both).await.unwrap_err();
    assert!(matches!(refused, PaykitError::Lib(_)), "{refused:?}");
    // A time deadline on a recurring request is the other kind's: refused too.
    let at = monthly(now, None).with_payment_deadline_ms(now + DAY_MS);
    let refused = shop.send_request(w.alice_id(), at).await.unwrap_err();
    assert!(matches!(refused, PaykitError::Lib(_)), "{refused:?}");
    assert!(shop.payee_activity().await.unwrap().is_empty());
    let sent = shop
        .send_request(
            w.alice_id(),
            monthly(now, None)
                .with_period_deadline_s(86_400)
                .with_per_period_endpoints(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let held = wallet.requests().await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(Some(held[0].request), sent.request);
    let value = crate::outcome_record::record_value(&mut wallet, &held[0].request)
        .await
        .unwrap();
    let stored =
        paykit_lib::records::StoredMessage::read(&value["request"], &w.alice_id()).unwrap();
    let paykit_lib::Body::PaymentRequest(body) = stored.body() else {
        panic!("{value}");
    };
    assert_eq!(
        body.payment_deadline,
        Some(paykit_lib::bodies::PaymentDeadline::PeriodStart { seconds: 86_400 })
    );
    assert!(body.per_period_endpoints);
}

/// Bob's signed monthly request to alice's apps, not delivered yet, and its hash.
fn signed_request(w: &World) -> (pubky_switch_core::message::MessagePlaintext, [u8; 32]) {
    let now = w.clock.now_ms();
    let body = paykit_lib::bodies::PaymentRequest {
        amount: btc("1"),
        payment_reference: PaymentReference::new("plan-1").unwrap(),
        proposal_expires_at: Some(now + DAY_MS),
        payment_deadline: None,
        recurrence: monthly(now, None).recurrence,
        destination: monthly(now, None).destination,
        per_period_endpoints: false,
        label: None,
        accepted_payment_endpoint_identifiers: None,
        conversion: None,
        metadata: None,
    }
    .to_body()
    .unwrap();
    let plaintext = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_request",
        Vec::new(),
        body,
    );
    let hash = paykit_lib::derive::message_hash(plaintext.json());
    (plaintext, hash)
}

/// Bob's shop's details for `period` of the request `hash`, entered at alice's switch for
/// `targets`.
fn inject_details(w: &World, targets: &[[u8; 16]], hash: [u8; 32], period: PeriodRef) {
    let body = details(period, "lnbc1p", w.clock.now_ms() + DAY_MS)
        .to_body()
        .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        targets,
        "paykit.payment_period_details",
        vec![hash],
        body,
    );
}

/// Bob's request `plaintext` reaches alice's `phone` only, which accepts it: the wallet
/// learns of the record from the shared records. Returns the request.
async fn accepted_by(
    w: &World,
    phone: &mut Paykit,
    plaintext: &pubky_switch_core::message::MessagePlaintext,
    hash: [u8; 32],
) -> RequestRef {
    w.deliver(&w.bob, "shop", &w.alice, &[phone.app_id()], plaintext);
    phone.receive(None).await.unwrap();
    let request = RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    phone.accept_subscription(&request).await.unwrap();
    request
}

#[tokio::test]
async fn test_periods_details_before_their_request_record_kept_then_placed() {
    // The details reach the wallet before the request they name: kept, then placed in the
    // period record once the request record, accepted by the phone, is held.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, january());
    wallet.receive(None).await.unwrap();
    let held = accepted_by(&w, &mut phone, &request, hash).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&held).await.unwrap();
    assert_eq!(view.periods.len(), 1, "{view:?}");
    assert_eq!(view.periods[0].details.len(), 1);
}

#[tokio::test]
async fn test_periods_details_naming_another_period_never_stored() {
    // Kept before the request, details naming a period that isn't the request's are
    // dropped once it comes; for a held request, so are they at once. Details for a request
    // the wallet rejected are taken with no effect.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, hash) = signed_request(&w);
    let mut off = january();
    off.ends_at -= 1;
    inject_details(&w, &[wallet.app_id()], hash, off);
    wallet.receive(None).await.unwrap();
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &request);
    wallet.receive(None).await.unwrap();
    inject_details(&w, &[wallet.app_id()], hash, off);
    wallet.receive(None).await.unwrap();
    let held = RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    assert!(wallet.request(&held).await.unwrap().periods.is_empty());
    wallet.reject_request(&held, None).await.unwrap();
    inject_details(&w, &[wallet.app_id()], hash, january());
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    assert!(wallet.request(&held).await.unwrap().periods.is_empty());
}

#[tokio::test]
async fn test_periods_details_for_a_record_not_listed_yet_held_until_listed() {
    // The wallet is narrowed to `contacts`; meanwhile bob's request comes and the phone
    // accepts it. The wallet is renewed to `pay` and `request` with no bundle, the `request`
    // key missing, so its records can't be
    // listed, and bob's details come. The request record's absence from the wallet's copy
    // decides nothing: the entry is held, not kept as details before their record. Once
    // the keys come and the records are listed, the details go into the period record.
    use crate::certificate::{list, renew, store_bundle};
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let first = wallet.app().cert().clone();
    let narrow = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &first, &narrow);
    wallet.receive(None).await.unwrap();
    let (request, sale) = subscribed(&w, &mut shop, &mut phone).await;
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        2_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &narrow, &wider);
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.scope_keys_awaited, "{summary:?}");
    let before = wallet.receive(None).await.unwrap().report.cursor;
    // The shop reads alice's directory again, which lists the wallet under its new
    // certificate, so the details go to it too.
    w.clock.advance_ms(11 * 60_000);
    shop.receive(None).await.unwrap();
    shop.send_period_details(
        &sale,
        details(january(), "lnbc1p", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        !received(&summary, "paykit.payment_period_details"),
        "{summary:?}"
    );
    assert_eq!(summary.report.cursor, before);
    store_bundle(&w, me, &wider);
    let mut taken = 0;
    for _ in 0..3 {
        let summary = wallet.receive(None).await.unwrap();
        taken += usize::from(received(&summary, "paykit.payment_period_details"));
    }
    assert_eq!(taken, 1);
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Subscribed);
    assert_eq!(view.periods.len(), 1, "{view:?}");
    assert_eq!(view.periods[0].details.len(), 1);
}

/// Alice's payer period record of `request`'s January.
fn payer_period(request: &RequestRef) -> RecordTarget {
    let collection = Collection::PayerActivity;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::period_record(
            &request.payee,
            &request.request_hash,
            january().start_secs(),
        ),
    )
}

/// `app` writes the value it reads at `target` back, at the data epoch it holds now.
async fn rewrites(app: &mut Paykit, target: &RecordTarget) {
    let held = app.app_mut().read_record(target).await.unwrap().unwrap();
    let value = held.content.value().unwrap().to_vec();
    let op = app
        .app_mut()
        .record_op(target, Some(&value), Vec::new())
        .await
        .unwrap();
    app.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_periods_details_over_a_period_record_awaiting_its_key_held_until_it_opens() {
    // Both of alice's apps hold the period record with bob's first details. The data epoch
    // rotates and the phone writes the record again at the new epoch, which the wallet holds
    // unopened for now. Bob's second details are then held, never written over a value the
    // wallet can't see, and placed beside the first once the key comes.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1a", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    let target = payer_period(&request);
    rewrites(&mut phone, &target).await;
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        matches!(
            held.content,
            RecordContent::Unreadable(Unopened::KeyNotHeld { .. })
        ),
        "{:?}",
        held.content
    );
    shop.send_period_details(&sale, details(january(), "lnbc1b", now + DAY_MS))
        .await
        .unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_period_details"));
    assert!(!summary.key_epochs_awaited.is_empty(), "{summary:?}");
    crate::unrequested::store_bundle(&w, wallet.app_id(), &wallet.app().cert().clone());
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.periods[0].details.len(), 2, "{view:?}");
}

#[tokio::test]
async fn test_periods_details_refused_for_good_written_at_a_later_receive() {
    // The switch refuses the details' record write for good: the message is kept, and
    // written into the period record by a later write of the wallet's own, in the same
    // receive.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    shop.send_period_details(
        &sale,
        details(january(), "lnbc1p", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        summary
            .report
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::EffectRefused { .. })),
        "{summary:?}"
    );
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.periods.len(), 1, "{view:?}");
    assert_eq!(view.periods[0].details.len(), 1);
}

/// A payment of 1 btc at the fake rail, and a payer's proof of it to `payee_app`'s invoice,
/// naming `period`. Returns the proof's body.
async fn period_proof(
    w: &World,
    payee_app: [u8; 16],
    period: PeriodRef,
) -> serde_json::Map<String, serde_json::Value> {
    let attempt = AttemptId::from_bytes([9; 16]);
    let outcome = w
        .rail
        .pay_outside(
            endpoint("btc-lightning-bolt11"),
            PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
            btc("1"),
            attempt,
        )
        .await
        .unwrap();
    let RailOutcome::Paid(paid) = outcome else {
        panic!("{outcome:?}");
    };
    PaymentProof {
        attempt_id: attempt,
        billing_period: Some(period),
        payment_endpoint_identifier: endpoint("btc-lightning-bolt11"),
        payment_app_id: payee_app,
        list_version: None,
        amount: None,
        allowance_id: None,
        label: None,
        payment_reference: None,
        proof: paid.proof,
    }
    .to_body(1)
    .unwrap()
}

#[tokio::test]
async fn test_periods_payee_takes_a_periods_proof_and_receipts_it() {
    // Alice pays January (a payment entered at the shop directly here): the proof goes into
    // the shop's period record, and the shop's receipt names the period.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
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
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert_eq!(sale.status, PayeeStatus::Accepted, "{sale:?}");
    assert_eq!(sale.periods.len(), 1);
    assert!(sale.periods[0].paid);
    let period = sale.periods[0].record;
    let receipt = shop.confirm_payment(&period).await.unwrap();
    assert!(receipt.is_some());
    let sale = &shop.payee_activity().await.unwrap()[0];
    let held = sale.periods[0].receipt.as_ref().unwrap();
    assert_eq!(held.billing_period, Some(january()));
    assert_eq!(held.request_hash, Some(request.request_hash));
}

#[tokio::test]
async fn test_periods_payee_drops_a_proof_naming_another_period() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let mut off = january();
    off.ends_at += 1;
    let body = period_proof(&w, shop.app_id(), off).await;
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
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert!(sale.periods.is_empty(), "{sale:?}");
    assert_eq!(sale.status, PayeeStatus::Accepted);
}

#[tokio::test]
async fn test_periods_proof_for_a_payee_record_not_listed_yet_held_until_listed() {
    // Alice's till sends bob a monthly request, which bob's wallet accepts; the till's
    // record is written while alice's wallet, holding `pay` only, can't read it. The wallet
    // is renewed to read `request` too, with no bundle, and bob's proof of January comes.
    // The record's absence decides nothing: the entry is held. Once the keys come and the
    // records are listed, the proof goes into the period record, once.
    use crate::certificate::{list, renew, store_bundle};
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut payer = w
        .join(&w.bob, "bwallet", AppScopes::new().pay().build().unwrap())
        .await;
    payer.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut till = w
        .join(
            &w.alice,
            "till",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = till
        .send_request(w.bob_id(), monthly(w.clock.now_ms(), None))
        .await
        .unwrap()
        .request
        .unwrap();
    payer.receive(None).await.unwrap();
    payer.accept_subscription(&request).await.unwrap();
    drop(till);
    wallet.receive(None).await.unwrap();
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.scope_keys_awaited, "{summary:?}");
    let before = wallet.receive(None).await.unwrap().report.cursor;
    let till_id = w.alice.identity().app_keys("till").app_id_bytes();
    let body = period_proof(&w, till_id, january()).await;
    w.inject(
        &w.bob,
        "bwallet",
        &w.alice,
        &[me],
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );
    let summary = wallet.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_proof"), "{summary:?}");
    assert_eq!(summary.report.cursor, before);
    store_bundle(&w, me, &wider);
    let mut proofs = 0;
    for _ in 0..3 {
        let summary = wallet.receive(None).await.unwrap();
        proofs += usize::from(received(&summary, "paykit.payment_proof"));
    }
    assert_eq!(proofs, 1);
    let sales = wallet.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].periods.len(), 1);
    assert!(sales[0].periods[0].paid);
}

#[tokio::test]
async fn test_periods_subscription_held_after_a_restore() {
    // Restored from a backup taken once subscribed, the wallet holds the subscription: the
    // payee may have cancelled it since the backup.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    app.resume_payments().await.unwrap();
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
}

#[tokio::test]
async fn test_periods_restore_takes_details_kept_before_their_record() {
    // A backup holds details kept before their request record; the restore takes them, and
    // they are placed once the request comes.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, january());
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let held = accepted_by(&w, &mut phone, &request, hash).await;
    app.receive(None).await.unwrap();
    app.receive(None).await.unwrap();
    let view = app.request(&held).await.unwrap();
    assert_eq!(view.periods.len(), 1, "{view:?}");
}

#[tokio::test]
async fn test_periods_acceptance_keeps_members_a_later_version_wrote() {
    // A later version's app wrote the request record with an attempted period and a member
    // this one doesn't know; the wallet's acceptance writes it again and keeps both.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let sent = shop
        .send_request(w.alice_id(), monthly(w.clock.now_ms(), None))
        .await
        .unwrap();
    let request = sent.request.unwrap();
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let target = crate::outcome_record::payer_record(&request);
    let held = phone.app_mut().read_record(&target).await.unwrap().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().unwrap()).unwrap();
    value["attempted_periods"] = json!([january().start_secs()]);
    value["future_member"] = json!({ "n": 1 });
    let bytes = serde_json::to_vec(&value).unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, Some(&bytes)).await;
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["response"].is_null(), "{value}");
    assert_eq!(value["attempted_periods"], json!([january().start_secs()]));
    assert_eq!(value["future_member"], json!({ "n": 1 }));
}

/// The request's Billing Period `k`: January 2026 for 0, to April for 3.
fn month(k: usize) -> PeriodRef {
    const STARTS: [u64; 5] = [0, 31, 59, 90, 120];
    let start = pubky_switch_client::testing::START_MS;
    PeriodRef {
        starts_at: start + STARTS[k] * DAY_MS,
        ends_at: start + STARTS[k + 1] * DAY_MS,
    }
}

/// Alice's wallet accepts bob's recurring request `draft`, and the shop reads the
/// acceptance; then the clock moves on `days`. Returns the request and the shop's record.
async fn subscribed_for(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    draft: RequestDraft,
    days: u64,
) -> (RequestRef, [u8; 32]) {
    let request = shop
        .send_request(w.alice_id(), draft)
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    w.clock.advance_ms(days * DAY_MS);
    (request, sale)
}

/// The shop sends details for `period`, and the wallet takes them, on a service time it
/// verifies as it receives. Returns whether the wallet stored them.
async fn details_stored(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    (request, sale): &(RequestRef, [u8; 32]),
    period: PeriodRef,
) -> bool {
    let expires_at = w.clock.now_ms() + DAY_MS;
    let msg_id = shop
        .send_period_details(sale, details(period, "lnbc1p", expires_at))
        .await
        .unwrap()
        .msg_id;
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    let view = wallet.request(request).await.unwrap();
    view.periods
        .iter()
        .any(|p| p.details.iter().any(|d| d.msg_id == msg_id))
}

#[tokio::test]
async fn test_periods_details_bound_current_period_taken() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    // In February.
    let sub = subscribed_for(&w, &mut shop, &mut wallet, monthly(now, None), 40).await;
    assert!(details_stored(&w, &mut shop, &mut wallet, &sub, month(1)).await);
}

#[tokio::test]
async fn test_periods_details_bound_next_period_taken() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let sub = subscribed_for(&w, &mut shop, &mut wallet, monthly(now, None), 40).await;
    assert!(details_stored(&w, &mut shop, &mut wallet, &sub, month(2)).await);
}

#[tokio::test]
async fn test_periods_details_bound_period_after_the_next_dropped() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let sub = subscribed_for(&w, &mut shop, &mut wallet, monthly(now, None), 40).await;
    // The shop sends none for April, after the next period.
    let refused = shop
        .send_period_details(&sub.1, details(month(3), "lnbc1p", now + 41 * DAY_MS))
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, PaykitError::Lib(paykit_lib::Error::Validation(_))),
        "{refused:?}"
    );
    // Sent all the same, the wallet takes them with no effect: no period record for April.
    inject_details(&w, &[wallet.app_id()], sub.0.request_hash, month(3));
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    assert!(wallet.request(&sub.0).await.unwrap().periods.is_empty());
}

#[tokio::test]
async fn test_periods_details_bound_period_past_its_deadline_dropped() {
    // Each period is due a day after its start. On day 40, January's and February's
    // deadlines have passed: the shop sends no details for them, and the wallet takes any
    // sent all the same with no effect. March, the next to start, takes them.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let draft = monthly(now, None).with_period_deadline_s(86_400);
    let sub = subscribed_for(&w, &mut shop, &mut wallet, draft, 40).await;
    refused_and_dropped(&w, &mut shop, &mut wallet, &sub, month(0)).await;
    refused_and_dropped(&w, &mut shop, &mut wallet, &sub, month(1)).await;
    assert!(details_stored(&w, &mut shop, &mut wallet, &sub, month(2)).await);
}

#[tokio::test]
async fn test_periods_details_bound_within_a_long_deadline_taken_past_sixteen() {
    // With a payment deadline, a started period whose deadline hasn't passed takes details,
    // however many periods started since: the bound of 16 is for a request without one.
    // Day 2 is the 19th latest to have started on day 20, and is due on day 32.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let draft = daily(now).with_period_deadline_s(30 * 86_400);
    let sub = subscribed_for(&w, &mut shop, &mut wallet, draft, 20).await;
    w.clock.advance_ms(HOUR_MS);
    assert!(details_stored(&w, &mut shop, &mut wallet, &sub, day(2)).await);
}

#[tokio::test]
async fn test_periods_details_window_deadlines_own_millisecond_taken() {
    // A period's payment deadline is the last instant an attempt on it is taken: the
    // attempt's `deadline` is the millisecond after it (Paykit spec 7.1). At January's
    // deadline itself, a day after its start, the payee sends its details and the payer
    // stores them; a millisecond later neither does.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let draft = monthly(now, None).with_period_deadline_s(86_400);
    let sub = subscribed_for(&w, &mut shop, &mut wallet, draft, 0).await;
    let deadline = january().starts_at + DAY_MS;
    w.clock.advance_ms(deadline - w.clock.now_ms());
    assert!(details_stored(&w, &mut shop, &mut wallet, &sub, month(0)).await);
    w.clock.advance_ms(1);
    let refused = shop
        .send_period_details(&sub.1, details(month(0), "lnbc1late", deadline + DAY_MS))
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, PaykitError::Lib(paykit_lib::Error::Validation(_))),
        "{refused:?}"
    );
    inject_details(&w, &[wallet.app_id()], sub.0.request_hash, month(0));
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    let view = wallet.request(&sub.0).await.unwrap();
    assert_eq!(view.periods[0].details.len(), 1, "{view:?}");
}

/// A daily request for 1 btc, from now, paid to one invoice of the sending app, its
/// proposal open a day.
fn daily(now: u64) -> RequestDraft {
    let mut endpoints = BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt11"),
        PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
    );
    let rule = AnchoredRule::new(now, 1, Unit::Day).unwrap();
    RequestDraft::new(
        btc("1"),
        PaymentReference::new("plan-1").unwrap(),
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(now + DAY_MS)
    .with_recurrence(Recurrence::new(rule, now, None).unwrap())
}

/// The daily request's Billing Period `k`, from January 1st 2026 (0).
fn day(k: u64) -> PeriodRef {
    let start = pubky_switch_client::testing::START_MS;
    PeriodRef {
        starts_at: start + k * DAY_MS,
        ends_at: start + (k + 1) * DAY_MS,
    }
}

/// A daily subscription, on its day 20 at 01:00: day 20 is the latest period to have
/// started, day 5 the 16th latest.
async fn on_day_20(w: &World, shop: &mut Paykit, wallet: &mut Paykit) -> (RequestRef, [u8; 32]) {
    let now = w.clock.now_ms();
    let sub = subscribed_for(w, shop, wallet, daily(now), 20).await;
    w.clock.advance_ms(HOUR_MS);
    sub
}

/// The shop's refusal of details for `period`, and the wallet taking them with no effect
/// when sent all the same: no period record for it.
async fn refused_and_dropped(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    (request, sale): &(RequestRef, [u8; 32]),
    period: PeriodRef,
) {
    let refused = shop
        .send_period_details(sale, details(period, "lnbc1p", w.clock.now_ms() + DAY_MS))
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, PaykitError::Lib(paykit_lib::Error::Validation(_))),
        "{refused:?}"
    );
    inject_details(w, &[wallet.app_id()], request.request_hash, period);
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    let view = wallet.request(request).await.unwrap();
    assert!(
        view.periods.iter().all(|p| p.billing_period != period),
        "{view:?}"
    );
}

#[tokio::test]
async fn test_periods_details_window_sixteenth_latest_started_taken() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let sub = on_day_20(&w, &mut shop, &mut wallet).await;
    assert!(details_stored(&w, &mut shop, &mut wallet, &sub, day(5)).await);
}

#[tokio::test]
async fn test_periods_details_window_seventeenth_latest_started_dropped() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let sub = on_day_20(&w, &mut shop, &mut wallet).await;
    refused_and_dropped(&w, &mut shop, &mut wallet, &sub, day(4)).await;
}

#[tokio::test]
async fn test_periods_details_window_next_period_taken() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let sub = on_day_20(&w, &mut shop, &mut wallet).await;
    assert!(details_stored(&w, &mut shop, &mut wallet, &sub, day(21)).await);
}

#[tokio::test]
async fn test_periods_details_window_period_after_the_next_dropped() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let sub = on_day_20(&w, &mut shop, &mut wallet).await;
    refused_and_dropped(&w, &mut shop, &mut wallet, &sub, day(22)).await;
}

#[tokio::test]
async fn test_periods_details_bound_without_a_deadline_started_periods_taken() {
    // A request with no payment deadline: only the start bounds its periods, so January's
    // details are taken in April.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let sub = subscribed_for(&w, &mut shop, &mut wallet, monthly(now, None), 100).await;
    assert!(details_stored(&w, &mut shop, &mut wallet, &sub, month(0)).await);
}

#[tokio::test]
async fn test_periods_details_bound_kept_before_the_record_far_period_dropped() {
    // Details kept before their request take the same bound once it comes: February's,
    // next in January, are placed; March's are dropped.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, month(1));
    inject_details(&w, &[wallet.app_id()], hash, month(2));
    wallet.receive(None).await.unwrap();
    let held = accepted_by(&w, &mut phone, &request, hash).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&held).await.unwrap();
    let periods: Vec<PeriodRef> = view.periods.iter().map(|p| p.billing_period).collect();
    assert_eq!(periods, [month(1)]);
}

/// Alice's wallet holding `pay` and `manage_contacts` on `lists`, inviting bob.
async fn contacts_wallet(w: &World, lists: &Arc<crate::common::ListsStore>) -> Paykit {
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(lists) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    wallet
}

#[tokio::test]
async fn test_periods_details_from_a_flagged_payee_stored_for_its_subscription() {
    // Details for a subscription the payer accepted update activity it holds with the
    // payee, so the spam rule doesn't gate them: a flagged payee's are stored.
    let w = World::new();
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut shop = w.shop().await;
    let mut wallet = contacts_wallet(&w, &lists).await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    lists.flag(&w.bob_id());
    shop.send_period_details(
        &sale,
        details(january(), "lnbc1p", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.periods.len(), 1, "{view:?}");
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 0);
}

#[tokio::test]
async fn test_periods_details_for_an_unaccepted_request_never_stored() {
    // No acceptance: details for its first period, and for periods years ahead, make no
    // period record.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (plaintext, hash) = signed_request(&w);
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &plaintext);
    wallet.receive(None).await.unwrap();
    let held = RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    let view = wallet.request(&held).await.unwrap();
    let periods = view.recurrence.unwrap().periods(40).unwrap();
    inject_details(&w, &[wallet.app_id()], hash, january());
    for p in periods.iter().skip(20) {
        inject_details(&w, &[wallet.app_id()], hash, PeriodRef::from(*p));
    }
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    let view = wallet.request(&held).await.unwrap();
    assert_eq!(view.status, RequestStatus::Open);
    assert!(view.periods.is_empty(), "{:?}", view.periods);
}

#[tokio::test]
async fn test_periods_kept_details_wait_while_the_epoch_key_is_missing() {
    // The wallet keeps details before the request record. The phone takes and accepts the
    // request and takes other details; the epoch rotates and the phone writes the period
    // record at the new epoch. The wallet's receive doesn't fail while it lacks the new
    // key, and once the key comes both details are held.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (plaintext, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, january());
    wallet.receive(None).await.unwrap();
    let request = accepted_by(&w, &mut phone, &plaintext, hash).await;
    inject_details(&w, &[phone.app_id()], hash, january());
    phone.receive(None).await.unwrap();
    assert_eq!(
        phone.request(&request).await.unwrap().periods[0]
            .details
            .len(),
        1
    );
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    let target = payer_period(&request);
    rewrites(&mut phone, &target).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    crate::unrequested::store_bundle(&w, wallet.app_id(), &wallet.app().cert().clone());
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.periods[0].details.len(), 2, "{view:?}");
}

#[tokio::test]
async fn test_periods_kept_details_for_a_rejected_request_never_placed() {
    // Details kept before their request record are dropped once it comes rejected, as at
    // receive.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (plaintext, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, january());
    wallet.receive(None).await.unwrap();
    w.deliver(&w.bob, "shop", &w.alice, &[phone.app_id()], &plaintext);
    phone.receive(None).await.unwrap();
    let request = RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    phone.reject_request(&request, None).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Rejected);
    assert!(view.periods.is_empty(), "{:?}", view.periods);
}

#[tokio::test]
async fn test_periods_one_payment_credits_one_period() {
    // One payment's evidence sent as the proof of January and of February: the first
    // confirmed takes the receipt, the other is not credited.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
    let mut second = body.clone();
    second.insert("billing_period".into(), month(1).to_json());
    for b in [body, second] {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop.app_id()],
            "paykit.payment_proof",
            vec![request.request_hash],
            b,
        );
    }
    shop.receive(None).await.unwrap();
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert_eq!(sale.periods.len(), 2);
    let mut receipts = 0;
    for p in sale.periods.clone() {
        receipts += usize::from(shop.confirm_payment(&p.record).await.unwrap().is_some());
    }
    assert_eq!(receipts, 1);
    // The other occurrence, paid in the same transaction, keeps its proof, shows no
    // receipt, and isn't credited when asked again.
    let sale = &shop.payee_activity().await.unwrap()[0];
    let credited = sale.periods.iter().find(|p| p.receipt.is_some()).unwrap();
    assert!(credited.paid && credited.credited);
    let other = sale.periods.iter().find(|p| p.receipt.is_none()).unwrap();
    assert!(other.paid);
    assert!(!other.credited);
    assert!(shop.verify_payment(&other.record).await.unwrap().is_none());
    assert!(shop.confirm_payment(&other.record).await.unwrap().is_none());
}

#[tokio::test]
async fn test_periods_accept_held_after_restore_waits_for_the_user() {
    // Restored from a backup taken before the acceptance, the request shows held: it is
    // accepted only once the user has seen the payee may have cancelled it, which ends the
    // hold.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    app.resume_payments().await.unwrap();
    app.receive(None).await.unwrap();
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
    let refused = app.accept_subscription(&request).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::HeldAfterRestore);
    let options = paykit_sdk::AcceptOptions::default()
        .with_possibly_cancelled_confirmed(pubky_switch_client::app::UserConfirmed::new());
    app.accept_subscription_with(&request, options)
        .await
        .unwrap();
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Subscribed
    );
}

#[tokio::test]
async fn test_periods_accept_a_blocked_payees_subscription_waits_for_the_user() {
    let w = World::new();
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut shop = w.shop().await;
    let mut wallet = contacts_wallet(&w, &lists).await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    wallet.block(&w.bob_id()).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Blocked
    );
    let refused = wallet.accept_subscription(&request).await.unwrap_err();
    let Refusal::Blocked { payment } = refusal(&refused) else {
        panic!("{refused:?}");
    };
    let options = paykit_sdk::AcceptOptions::default().with_blocked_confirmed(*payment);
    wallet
        .accept_subscription_with(&request, options)
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Accepted
    );
}

#[tokio::test]
async fn test_periods_proof_naming_earlier_details_credited_at_the_payee() {
    // The payer may pay by any valid details (Paykit spec 8.2): the shop's period record
    // holds two; a proof naming the earlier one is stored with them current, and the
    // period shows paid.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1invoice", now + DAY_MS))
        .await
        .unwrap();
    w.clock.advance_ms(1_000);
    shop.send_period_details(&sale, details(january(), "lnbc1later", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let fresh = wallet
        .app_mut()
        .read_record(&payer_period(&request))
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(fresh.content.value().unwrap()).unwrap();
    let earlier =
        paykit_lib::records::StoredMessage::read(&value["earlier_details"][0], &w.alice_id())
            .unwrap();
    let body = period_proof(&w, shop.app_id(), january()).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, earlier.hash()],
        body,
    );
    shop.receive(None).await.unwrap();
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert!(sale.periods[0].paid, "{sale:?}");
    // The details it names are the current ones, the later ones earlier.
    assert_eq!(
        sale.periods[0].details.last().unwrap().msg_id,
        earlier.message().msg_id
    );
    assert!(shop
        .confirm_payment(&sale.periods[0].record)
        .await
        .unwrap()
        .is_some());
}

/// The signer stores bob's app `app`'s key bundle for `cert`, every data epoch up to the
/// current.
fn store_bob_bundle(w: &World, app: [u8; 16], cert: &pubky_switch_core::records::AppCert) {
    store_bob_bundle_from(w, app, cert, 1);
}

/// The signer stores bob's app `app`'s key bundle for `cert`, the data epochs from `first`
/// to the current.
fn store_bob_bundle_from(
    w: &World,
    app: [u8; 16],
    cert: &pubky_switch_core::records::AppCert,
    first: u64,
) {
    use pubky_switch_client::app::Protocol as _;
    let epoch = w.bob.switch().identity_doc().unwrap().data_epoch();
    let protocol = paykit_sdk::PaykitProtocol::new();
    let parts = pubky_switch_core::records::seal_bundle_parts(
        w.bob.identity().key(),
        cert,
        first,
        epoch,
        &|scope| protocol.has_keys(scope),
    )
    .unwrap();
    for part in parts {
        w.bob
            .switch()
            .signer_op(pubky_switch_core::ops::OpKind::KeyBundlePut(
                pubky_switch_core::ops::KeyBundlePut {
                    target_app_id: app,
                    bundle: part.bundle,
                    first_epoch: part.first_epoch,
                    last_epoch: part.last_epoch,
                },
            ))
            .unwrap();
    }
}

/// Bob's payee period record of `request`'s January.
fn payee_period(request: &RequestRef) -> RecordTarget {
    payee_period_at(request, january())
}

/// Bob's payee period record of `request`'s `period`.
fn payee_period_at(request: &RequestRef, period: PeriodRef) -> RecordTarget {
    let collection = Collection::PayeeActivity;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::period_record(&request.payee, &request.request_hash, period.start_secs()),
    )
}

#[tokio::test]
async fn test_periods_send_details_over_a_period_record_awaiting_its_key_refused_for_now() {
    // Bob's till writes the shop's period record again at a data epoch the shop holds no
    // key of yet: the shop's next details are refused for now, never written over a value
    // it can't see.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1a", now + DAY_MS))
        .await
        .unwrap();
    till.receive(None).await.unwrap();
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bob_bundle(&w, till.app_id(), &till.app().cert().clone());
    till.receive(None).await.unwrap();
    rewrites(&mut till, &payee_period(&request)).await;
    shop.receive(None).await.unwrap();
    let refused = shop
        .send_period_details(&sale, details(january(), "lnbc1b", now + DAY_MS))
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NotNow(_)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_periods_send_details_for_a_rejected_request_refused() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    wallet.reject_request(&request, None).await.unwrap();
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    let refused = shop
        .send_period_details(
            &sale,
            details(january(), "lnbc1p", w.clock.now_ms() + DAY_MS),
        )
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Rejected);
}

#[tokio::test]
async fn test_periods_details_for_a_period_shown_paid_refused() {
    // The payee sends details only for a period whose record doesn't show it paid (Paykit
    // spec 8.2): once the period record holds the proof, and then its receipt, no later
    // details go, and the details the proof names stay current.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    let named = shop
        .send_period_details(&sale, details(january(), "lnbc1invoice", now + DAY_MS))
        .await
        .unwrap()
        .msg_id;
    let target = payee_period(&request);
    let held = shop.app_mut().read_record(&target).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    let stored =
        paykit_lib::records::StoredMessage::read(&value["period_details"], &w.alice_id()).unwrap();
    let body = period_proof(&w, shop.app_id(), january()).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, stored.hash()],
        body,
    );
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(1_000);
    let refused = shop
        .send_period_details(&sale, details(january(), "lnbc1later", now + DAY_MS))
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::AlreadyPaid);
    let period = shop.payee_activity().await.unwrap()[0].periods[0].record;
    assert!(shop.confirm_payment(&period).await.unwrap().is_some());
    let refused = shop
        .send_period_details(&sale, details(january(), "lnbc1late", now + DAY_MS))
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::AlreadyPaid);
    let held = shop.app_mut().read_record(&target).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    let current =
        paykit_lib::records::StoredMessage::read(&value["period_details"], &w.alice_id()).unwrap();
    assert_eq!(current.message().msg_id, named);
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert!(sale.periods[0].paid && sale.periods[0].credited);
    assert_eq!(sale.periods[0].details.len(), 1);
}

/// A payment of 1 btc at the fake rail to `invoice`, attempt `attempt`, and a payer's proof
/// of it to `payee_app`, naming `period`. Returns the proof's body.
async fn proof_to(
    w: &World,
    payee_app: [u8; 16],
    period: PeriodRef,
    invoice: &str,
    attempt: u8,
) -> serde_json::Map<String, serde_json::Value> {
    let attempt = AttemptId::from_bytes([attempt; 16]);
    let outcome = w
        .rail
        .pay_outside(
            endpoint("btc-lightning-bolt11"),
            PaymentEndpointPayload::new(invoice).unwrap(),
            btc("1"),
            attempt,
        )
        .await
        .unwrap();
    let RailOutcome::Paid(paid) = outcome else {
        panic!("{outcome:?}");
    };
    PaymentProof {
        attempt_id: attempt,
        billing_period: Some(period),
        payment_endpoint_identifier: endpoint("btc-lightning-bolt11"),
        payment_app_id: payee_app,
        list_version: None,
        amount: None,
        allowance_id: None,
        label: None,
        payment_reference: None,
        proof: paid.proof,
    }
    .to_body(1)
    .unwrap()
}

#[tokio::test]
async fn test_periods_verify_takes_the_endpoint_of_the_named_details() {
    // The payment went to the invoice of the details the proof names, not the request's:
    // verified against that invoice, and credited.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    shop.send_period_details(
        &sale,
        details(january(), "lnbc1details", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    let held = shop
        .app_mut()
        .read_record(&payee_period(&request))
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    let named =
        paykit_lib::records::StoredMessage::read(&value["period_details"], &w.alice_id()).unwrap();
    let body = proof_to(&w, shop.app_id(), january(), "lnbc1details", 5).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, named.hash()],
        body,
    );
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].periods[0].record;
    assert!(shop.verify_payment(&record).await.unwrap().is_some());
}

#[tokio::test]
async fn test_periods_per_period_endpoints_serve_only_the_first_period() {
    // With per-period endpoints, the request's invoice pays January, and a payment of
    // February to it isn't credited.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let draft = monthly(now, None).with_per_period_endpoints();
    let (request, _) = subscribed_for(&w, &mut shop, &mut wallet, draft, 40).await;
    for (period, attempt) in [(month(0), 6), (month(1), 7)] {
        let body = proof_to(&w, shop.app_id(), period, "lnbc1invoice", attempt).await;
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop.app_id()],
            "paykit.payment_proof",
            vec![request.request_hash],
            body,
        );
    }
    shop.receive(None).await.unwrap();
    let periods = shop.payee_activity().await.unwrap()[0].periods.clone();
    assert_eq!(periods.len(), 2);
    assert!(shop
        .verify_payment(&periods[0].record)
        .await
        .unwrap()
        .is_some());
    assert!(shop
        .verify_payment(&periods[1].record)
        .await
        .unwrap()
        .is_none());
    // March paid to the destination of the details its proof names is credited.
    let sale = shop.payee_activity().await.unwrap()[0].record;
    shop.send_period_details(
        &sale,
        details(month(2), "lnbc1march", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    let held = shop
        .app_mut()
        .read_record(&payee_period_at(&request, month(2)))
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    let named =
        paykit_lib::records::StoredMessage::read(&value["period_details"], &w.alice_id()).unwrap();
    let body = proof_to(&w, shop.app_id(), month(2), "lnbc1march", 8).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, named.hash()],
        body,
    );
    shop.receive(None).await.unwrap();
    let periods = shop.payee_activity().await.unwrap()[0].periods.clone();
    let march = periods
        .iter()
        .find(|p| p.billing_period == month(2))
        .unwrap();
    assert!(shop.verify_payment(&march.record).await.unwrap().is_some());
}

#[tokio::test]
async fn test_periods_unaccepted_expires_the_millisecond_after_its_proposal_expiry() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    let expires_at = w.clock.now_ms() + DAY_MS;
    w.clock.advance_ms(expires_at - w.clock.now_ms());
    wallet.app_mut().read_claim(&[0; 32]).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    w.clock.advance_ms(1);
    wallet.app_mut().read_claim(&[0; 32]).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Expired
    );
}

#[tokio::test]
async fn test_periods_accept_a_subscription_whose_last_period_ended_shows_ended() {
    // As in v0.1, a recurrence past its end is still accepted; it shows ended.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let draft =
        monthly(now, Some(now + 40 * DAY_MS)).with_proposal_expires_at_ms(now + 90 * DAY_MS);
    let request = shop
        .send_request(w.alice_id(), draft)
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60 * DAY_MS);
    wallet.app_mut().read_claim(&[0; 32]).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Ended
    );
}

#[tokio::test]
async fn test_periods_accept_and_details_kept_reported_sent() {
    // A `503` keeps the acceptance's batch, and then the details': each call reports its
    // message kept, not an error, and each goes once at the next write or read, so a host
    // that doesn't call again sends no second one.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    let unavailable = || pubky_switch_client::testing::Fault::Error {
        path: "/ops".into(),
        code: pubky_switch_core::http::ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    };
    w.alice.switch().fail_next(unavailable());
    let accepted = wallet.accept_subscription(&request).await.unwrap();
    assert!(
        matches!(accepted.delivery, paykit_sdk::MessageDelivery::Kept { .. }),
        "{accepted:?}"
    );
    wallet.receive(None).await.unwrap();
    wallet.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Accepted
    );
    w.bob.switch().fail_next(unavailable());
    let sent = shop
        .send_period_details(
            &sale,
            details(january(), "lnbc1p", w.clock.now_ms() + DAY_MS),
        )
        .await
        .unwrap();
    assert!(
        matches!(sent.delivery, paykit_sdk::MessageDelivery::Kept { .. }),
        "{sent:?}"
    );
    shop.receive(None).await.unwrap();
    shop.flush().await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.periods.len(), 1, "{view:?}");
    assert_eq!(view.periods[0].details.len(), 1);
    assert_eq!(view.periods[0].details[0].msg_id, sent.msg_id);
}

#[tokio::test]
async fn test_periods_proof_naming_details_not_held_kept_not_dropped() {
    // A proof naming period details the shop doesn't hold is taken and kept, to be placed
    // once the period record holds them: placed now, the record wouldn't bind.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, [1; 32]],
        body,
    );
    let summary = shop.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_proof"), "{summary:?}");
    shop.receive(None).await.unwrap();
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert!(sale.periods.is_empty(), "{sale:?}");
}

#[tokio::test]
async fn test_periods_details_refused_for_good_dropped_once_the_payee_cancelled() {
    // The details' record write is refused for good and kept to write later; the payee's
    // cancellation that follows is kept as a stop. The later write takes the details with
    // no effect: a subscription that runs no more has no periods to pay.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    shop.send_period_details(
        &sale,
        details(january(), "lnbc1p", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    shop.cancel_sent_request(&sale, None).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        summary
            .report
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::EffectRefused { .. })),
        "{summary:?}"
    );
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Cancelled);
    assert!(view.periods.is_empty(), "{view:?}");
}

#[tokio::test]
async fn test_periods_accept_after_another_app_accepted_unseen_refused_as_subscribed() {
    // The phone accepts; the wallet, whose copy doesn't show that yet, accepts too: its
    // batch meets the newer version, and the record read again shows the subscription.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    phone.accept_subscription(&request).await.unwrap();
    let refused = wallet.accept_subscription(&request).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Subscribed);
}

/// Bob's shop and till take alice's proof of January; the till writes January's period
/// record again at a data epoch the shop holds no key of yet; a proof of February, another
/// payment, then reaches the shop. Returns the shop and February's record.
async fn with_a_sealed_record(w: &World) -> (Paykit, [u8; 32]) {
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(w, &mut shop, &mut wallet).await;
    till.receive(None).await.unwrap();
    let body = proof_to(w, shop.app_id(), month(0), "lnbc1invoice", 6).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id(), till.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );
    shop.receive(None).await.unwrap();
    till.receive(None).await.unwrap();
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bob_bundle(w, till.app_id(), &till.app().cert().clone());
    till.receive(None).await.unwrap();
    rewrites(&mut till, &payee_period_at(&request, month(0))).await;
    // The shop gets the newest epoch's key only: it writes there, and can't open January.
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.bob.switch().identity_doc().unwrap().data_epoch();
    store_bob_bundle_from(w, shop.app_id(), &shop.app().cert().clone(), newest);
    shop.receive(None).await.unwrap();
    let body = proof_to(w, shop.app_id(), month(1), "lnbc1invoice", 7).await;
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
    let key = shop
        .app()
        .record_key(&payee_period_at(&request, month(0)))
        .unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        matches!(
            held.content,
            RecordContent::Unreadable(Unopened::KeyNotHeld { .. })
        ),
        "{:?}",
        held.content
    );
    let key = shop
        .app()
        .record_key(&payee_period_at(&request, month(1)))
        .unwrap();
    (shop, key)
}

#[tokio::test]
async fn test_periods_credit_held_while_a_record_is_sealed_then_given_once_it_opens() {
    // A record the shop can't open may credit this payment: nothing is credited or
    // receipted until it opens.
    let w = World::new();
    let (mut shop, february) = with_a_sealed_record(&w).await;
    let held = shop.confirm_payment(&february).await.unwrap_err();
    assert_eq!(refusal(&held), Refusal::MaybeCredited);
    let refused = shop.verify_payment(&february).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::MaybeCredited);
    store_bob_bundle(&w, shop.app_id(), &shop.app().cert().clone());
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.confirm_payment(&february).await.unwrap().is_some());
}

#[tokio::test]
async fn test_periods_credit_given_on_the_users_confirmation_while_a_record_is_sealed() {
    let w = World::new();
    let (mut shop, february) = with_a_sealed_record(&w).await;
    let confirmed = paykit_sdk::ReceiptDetails::new()
        .with_maybe_credited_confirmed(pubky_switch_client::app::UserConfirmed::new());
    assert!(shop
        .confirm_payment_with(&february, confirmed)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_periods_credit_not_given_while_the_records_cant_be_listed_then_given() {
    // Restored, the shop's copy of its records lacks records until it lists them: a
    // listing that fails is the credit's error, nothing credited, and once it lists, the
    // payment is credited.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
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
    let record = shop.payee_activity().await.unwrap()[0].periods[0].record;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/records".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    assert!(shop.confirm_payment(&record).await.is_err());
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
}

#[tokio::test]
async fn test_periods_kept_details_wait_over_a_record_sealed_at_an_older_key() {
    // The wallet holds the current key but not the older one the phone sealed the period
    // record at: kept details wait, never written over it, and once the key comes both
    // details are held.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (plaintext, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, january());
    wallet.receive(None).await.unwrap();
    let request = accepted_by(&w, &mut phone, &plaintext, hash).await;
    inject_details(&w, &[phone.app_id()], hash, january());
    phone.receive(None).await.unwrap();
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    rewrites(&mut phone, &payer_period(&request)).await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle_from(&w, wallet.app_id(), &cert, newest);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.periods[0].details.len(), 2, "{view:?}");
}

#[tokio::test]
async fn test_periods_details_after_a_kept_payee_cancellation_never_stored() {
    // The payee's cancellation is kept as a stop, its placement refused for now; details
    // sent after it are taken with no effect, as for a cancellation the record holds.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    shop.cancel_sent_request(&sale, None).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    wallet.receive(None).await.unwrap();
    inject_details(&w, &[wallet.app_id()], request.request_hash, january());
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Cancelled);
    assert!(view.periods.is_empty(), "{view:?}");
}

#[tokio::test]
async fn test_periods_kept_details_after_a_kept_payee_cancellation_never_placed() {
    // Details kept before their record; the phone accepts, the payee cancels, and the
    // wallet's placement of the cancellation is refused for now: the kept details are
    // dropped, the stop kept counting as the cancellation.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (plaintext, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, january());
    wallet.receive(None).await.unwrap();
    let request = accepted_by(&w, &mut phone, &plaintext, hash).await;
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_request_cancellation",
        vec![hash],
        serde_json::Map::new(),
    );
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Cancelled);
    assert!(view.periods.is_empty(), "{view:?}");
}

#[tokio::test]
async fn test_periods_accept_whose_delivery_fails_now_reported_queued() {
    // The acceptance is recorded and queued; a try to deliver it now that fails is no
    // error: it stays queued for the next flush.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/inbox".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    let accepted = wallet.accept_subscription(&request).await.unwrap();
    assert!(
        matches!(
            accepted.delivery,
            paykit_sdk::MessageDelivery::Queued { .. }
        ),
        "{accepted:?}"
    );
    wallet.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Accepted
    );
}

#[tokio::test]
async fn test_periods_kept_details_not_bound_to_their_key_refused_as_corrupt() {
    // Two kept details swapped between their keys: each names another `msg_id` than its
    // key. Read back as a restore reads them, they are corrupt state: never placed.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (plaintext, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, january());
    inject_details(&w, &[wallet.app_id()], hash, month(1));
    wallet.receive(None).await.unwrap();
    let kept = wallet.app().protocol_scan(b"pr/").await.unwrap();
    assert_eq!(kept.len(), 2);
    wallet
        .app_mut()
        .protocol_commit(&[
            pubky_switch_client::app::LocalWrite::Put {
                key: kept[0].0.clone(),
                value: kept[1].1.clone(),
            },
            pubky_switch_client::app::LocalWrite::Put {
                key: kept[1].0.clone(),
                value: kept[0].1.clone(),
            },
        ])
        .await
        .unwrap();
    let request = accepted_by(&w, &mut phone, &plaintext, hash).await;
    let first = wallet.receive(None).await;
    let second = wallet.receive(None).await;
    assert!(first.is_err() || second.is_err(), "{first:?} / {second:?}");
    let view = wallet.request(&request).await.unwrap();
    assert!(view.periods.is_empty(), "{view:?}");
}

#[tokio::test]
async fn test_periods_details_over_a_corrupt_kept_stop_not_dropped() {
    // A kept stop that doesn't read is corrupt state, never a stop that drops the details
    // as taken with no effect.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let record_key = wallet
        .app()
        .record_key(&crate::outcome_record::payer_record(&request))
        .unwrap();
    let key = format!("kc/{}", pubky_switch_core::enc::b64(&record_key));
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: key.into_bytes(),
            value: b"not a kept cancellation".to_vec(),
        }])
        .await
        .unwrap();
    shop.send_period_details(
        &sale,
        details(january(), "lnbc1p", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    let summary = wallet.receive(None).await;
    assert!(
        summary
            .as_ref()
            .map_or(true, |s| !received(s, "paykit.payment_period_details")),
        "{summary:?}"
    );
    // The entry is kept for the host, not consumed.
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_periods_credit_never_given_when_a_record_read_fails() {
    // The first period is credited; the second, paid in the same transaction, is confirmed
    // by the user while one held record read fails, whichever: a failed read is the call's
    // error, never "no other credit", so the payment is never credited twice.
    let w = World::new();
    let failing = Arc::new(crate::common::FailingRecordReads::default());
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&failing) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
    let mut second = body.clone();
    second.insert("billing_period".into(), month(1).to_json());
    for b in [body, second] {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop.app_id()],
            "paykit.payment_proof",
            vec![request.request_hash],
            b,
        );
    }
    shop.receive(None).await.unwrap();
    let periods = shop.payee_activity().await.unwrap()[0].periods.clone();
    assert!(shop
        .confirm_payment(&periods[0].record)
        .await
        .unwrap()
        .is_some());
    for n in 1..=12 {
        failing.fail_nth(n);
        let confirmed = paykit_sdk::ReceiptDetails::new()
            .with_maybe_credited_confirmed(pubky_switch_client::app::UserConfirmed::new());
        let credited = shop
            .confirm_payment_with(&periods[1].record, confirmed)
            .await;
        failing.heal();
        assert!(!matches!(credited, Ok(Some(_))), "read {n}: {credited:?}");
    }
}

#[tokio::test]
async fn test_periods_accept_after_a_rejection_or_a_cancellation_refused() {
    // After a rejection the payer neither accepts nor cancels; after the payee's
    // cancellation it sends no response.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let rejected = requested(&w, &mut shop, &mut wallet).await;
    wallet.reject_request(&rejected, None).await.unwrap();
    let refused = wallet.accept_subscription(&rejected).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Rejected);
    let cancelled = requested(&w, &mut shop, &mut wallet).await;
    let sale = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.request_hash == Some(cancelled.request_hash))
        .unwrap()
        .record;
    shop.cancel_sent_request(&sale, None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let refused = wallet.accept_subscription(&cancelled).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Cancelled);
    let refused = wallet.reject_request(&cancelled, None).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Cancelled);
}

#[tokio::test]
async fn test_periods_no_response_while_the_request_record_is_sealed() {
    // The phone writes the request record again at a key the wallet doesn't hold, though it
    // holds a later one: the wallet can't open it, so it sends no response for now.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    rewrites(&mut phone, &crate::outcome_record::payer_record(&request)).await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle_from(&w, wallet.app_id(), &cert, newest);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let refused = wallet.accept_subscription(&request).await.unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NotNow(_)),
        "{refused:?}"
    );
    let refused = wallet.reject_request(&request, None).await.unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NotNow(_)),
        "{refused:?}"
    );
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
}

#[tokio::test]
async fn test_periods_another_payers_credit_of_a_settlement_leaves_the_payment_credited() {
    // Carol's proof of the same settlement is credited first, as a payment without a
    // request: one payment credits one occurrence per payer, so alice's period is still
    // credited, carol's record not being of her.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let _carol = w
        .join(&w.carol, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
    let mut unrequested = body.clone();
    unrequested.remove("billing_period");
    unrequested.insert("amount".into(), json!({ "asset": "btc", "value": "1" }));
    w.inject(
        &w.carol,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        Vec::new(),
        unrequested,
    );
    shop.receive(None).await.unwrap();
    let carols = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.payer == w.carol_id())
        .unwrap()
        .record;
    assert!(shop.confirm_payment(&carols).await.unwrap().is_some());
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
    let sale = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.request_hash == Some(request.request_hash))
        .unwrap();
    assert!(shop
        .confirm_payment(&sale.periods[0].record)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_periods_send_details_never_writes_over_a_period_record_that_doesnt_read() {
    // The till writes the shop's period record as a value no reader reads, which may hold
    // what a later version stored (Paykit spec 8.1). The shop, whose copy refuses that
    // write, is refused its next details for now, and the value stays as it is.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1a", now + DAY_MS))
        .await
        .unwrap();
    till.receive(None).await.unwrap();
    let target = payee_period(&request);
    let held = till.app_mut().read_record(&target).await.unwrap().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().unwrap()).unwrap();
    value["v"] = json!(99);
    let bytes = serde_json::to_vec(&value).unwrap();
    let op = till
        .app_mut()
        .record_op(&target, Some(&bytes), Vec::new())
        .await
        .unwrap();
    till.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let refused = shop
        .send_period_details(&sale, details(january(), "lnbc1b", now + DAY_MS))
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NotNow(_)),
        "{refused:?}"
    );
    // What the switch holds, as the till reads it: its own write, not written over.
    let after = till.app_mut().read_record(&target).await.unwrap().unwrap();
    let after: serde_json::Value = serde_json::from_slice(after.content.value().unwrap()).unwrap();
    assert_eq!(after["v"], json!(99), "{after}");
}

#[tokio::test]
async fn test_periods_kept_details_never_written_over_a_period_record_that_doesnt_read() {
    // The details reach the wallet before their request record and are kept. The phone
    // accepts, and writes the payer's period record as a value no reader reads: once the
    // request record is held, the kept details wait, never written over that value.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, january());
    wallet.receive(None).await.unwrap();
    let held = accepted_by(&w, &mut phone, &request, hash).await;
    let target = payer_period(&held);
    let bytes = serde_json::to_vec(&json!({ "v": 99 })).unwrap();
    let op = phone
        .app_mut()
        .record_op(&target, Some(&bytes), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let after = wallet
        .app_mut()
        .read_record(&target)
        .await
        .unwrap()
        .unwrap();
    let after: serde_json::Value = serde_json::from_slice(after.content.value().unwrap()).unwrap();
    assert_eq!(after, json!({ "v": 99 }));
    assert!(wallet.request(&held).await.unwrap().periods.is_empty());
}

#[tokio::test]
async fn test_periods_details_over_a_request_record_that_doesnt_read_not_lost() {
    // The phone writes the payer's request record as a value no reader reads; the shop's
    // details then reach the wallet. No effect is decided from a record the app can't read
    // (Paykit spec 8.1): once the record reads again, the details are stored.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    let target = crate::outcome_record::payer_record(&request);
    let held = phone.app_mut().read_record(&target).await.unwrap().unwrap();
    let good = held.content.value().unwrap().to_vec();
    let mut value: serde_json::Value = serde_json::from_slice(&good).unwrap();
    value["v"] = json!(99);
    let bad = serde_json::to_vec(&value).unwrap();
    let op = phone
        .app_mut()
        .record_op(&target, Some(&bad), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let now = w.clock.now_ms();
    let sent = shop
        .send_period_details(&sale, details(january(), "lnbc1a", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let op = phone
        .app_mut()
        .record_op(&target, Some(&good), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert!(
        view.periods
            .iter()
            .any(|p| p.details.iter().any(|d| d.msg_id == sent.msg_id)),
        "{view:?}"
    );
}

#[tokio::test]
async fn test_periods_credit_given_beside_a_record_under_another_policy() {
    // A `payee_activity` record under a policy that isn't Paykit's is ignored, as every
    // reader ignores it (Paykit wire 5.1): it can't hold a credit, so the payment is
    // credited without the user's confirmation.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
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
    let record = shop.payee_activity().await.unwrap()[0].periods[0].record;
    let mut policy = Collection::PayeeActivity.fixed_policy();
    policy.write_scopes = vec!["pay".into(), "request".into()];
    let foreign = RecordTarget::new(
        SwitchCollection::new(Collection::PayeeActivity.name(), policy),
        b"another protocol's record",
    );
    let bytes = serde_json::to_vec(&json!({ "v": 1 })).unwrap();
    let op = shop
        .app_mut()
        .record_op(&foreign, Some(&bytes), Vec::new())
        .await
        .unwrap();
    shop.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    let held = shop.app_mut().read_record(&foreign).await.unwrap().unwrap();
    assert!(held.content.value().is_some(), "{held:?}");
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
}

#[tokio::test]
async fn test_periods_ended_subscription_of_a_blocked_payee_shows_blocked() {
    // `Ended` is a view of the time: the deny list overrides it as it does an open
    // request's status, as do a kept stop and the restore's hold.
    let w = World::new();
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut shop = w.shop().await;
    let mut wallet = contacts_wallet(&w, &lists).await;
    let now = w.clock.now_ms();
    let request = shop
        .send_request(w.alice_id(), monthly(now, Some(now + 40 * DAY_MS)))
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    w.clock.advance_ms(60 * DAY_MS);
    wallet.app_mut().read_claim(&[0; 32]).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Ended
    );
    wallet.block(&w.bob_id()).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Blocked
    );
}

#[tokio::test]
async fn test_periods_details_expiry_judged_on_the_verified_service_time() {
    // The shop's device clock runs two hours ahead of the service's. Details valid for an
    // hour more on the service time it verified are sent: the device clock never decides.
    let w = World::new();
    let shop_store = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&shop_store),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let app_id = shop.app_id();
    drop(shop);
    let ahead = Arc::new(pubky_switch_client::clock::TestClock::new(
        w.clock.now_ms() + 2 * HOUR_MS,
    ));
    let mut shop = w.load_on_clock(&w.bob, shop_store, app_id, ahead).await;
    let expires_at = w.clock.now_ms() + HOUR_MS;
    let sent = shop
        .send_period_details(&sale, details(january(), "lnbc1p", expires_at))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert!(
        view.periods
            .iter()
            .any(|p| p.details.iter().any(|d| d.msg_id == sent.msg_id)),
        "{view:?}"
    );
}

#[tokio::test]
async fn test_periods_acceptance_stays_after_a_restore() {
    // Restored from a backup taken after the acceptance, the wallet holds the subscription,
    // and once the user has seen the payee may have cancelled it, a second acceptance is
    // refused: the request record keeps the acceptance, and no second one goes.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    app.resume_payments().await.unwrap();
    app.receive(None).await.unwrap();
    let options = paykit_sdk::AcceptOptions::default()
        .with_possibly_cancelled_confirmed(pubky_switch_client::app::UserConfirmed::new());
    let refused = app
        .accept_subscription_with(&request, options)
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Subscribed);
    let summary = shop.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_request_acceptance"));
}

/// Bob's `till`, alice's `wallet` subscribed to its monthly request, sends her January's
/// details twice, `apart_ms` apart: its period record keeps the first in `earlier_details`
/// and the second in `period_details`. Returns the request, the till's record of it, and
/// the two `msg_id`s, the earlier first.
async fn details_twice(
    w: &World,
    till: &mut Paykit,
    wallet: &mut Paykit,
    apart_ms: u64,
) -> (RequestRef, [u8; 32], [String; 2]) {
    let request = requested(w, till, wallet).await;
    wallet.accept_subscription(&request).await.unwrap();
    till.receive(None).await.unwrap();
    let sale = till.payee_activity().await.unwrap()[0].record;
    let first = till
        .send_period_details(
            &sale,
            details(january(), "lnbc1first", w.clock.now_ms() + DAY_MS),
        )
        .await
        .unwrap();
    w.clock.advance_ms(apart_ms);
    let second = till
        .send_period_details(
            &sale,
            details(january(), "lnbc1second", w.clock.now_ms() + DAY_MS),
        )
        .await
        .unwrap();
    let period = &till.payee_activity().await.unwrap()[0].periods[0];
    let held: Vec<&str> = period.details.iter().map(|d| d.msg_id.as_str()).collect();
    assert_eq!(held, [second.msg_id.as_str(), first.msg_id.as_str()]);
    wallet.receive(None).await.unwrap();
    (request, sale, [first.msg_id, second.msg_id])
}

/// `backup` restored onto a new store as an app of bob's, without a receive: no outbox holds
/// what it sent before.
pub(crate) async fn bob_restored(w: &World, backup: &paykit_sdk::PaykitBackup) -> Paykit {
    Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

/// Whether the client of `app` holds `msg_id` queued or sent.
pub(crate) async fn queued(app: &Paykit, msg_id: &str) -> bool {
    app.message_delivery(msg_id).await.unwrap().is_some()
}

#[tokio::test]
async fn test_periods_restore_queues_each_own_details_again_once() {
    // The period record keeps both details the shop sent: the restored shop queues each
    // again, once, the earlier one as well as the current one (Paykit spec 7.2, "What a
    // restore loses").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (_, _, sent) = details_twice(&w, &mut shop, &mut wallet, 1_000).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = bob_restored(&w, &backup).await;
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await);
    }
    let before = w.alice.switch().entries_from(&w.bob_id());
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_none());
    for msg_id in &sent {
        assert!(queued(&shop, msg_id).await, "{msg_id}");
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + 2);
    // Once: a later receive queues nothing more.
    shop.receive(None).await.unwrap();
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + 2);
    // The wallet takes the copies as the details it holds.
    wallet.receive(None).await.unwrap();
    let view = &wallet.requests().await.unwrap()[0];
    assert_eq!(view.periods.len(), 1);
    assert_eq!(view.periods[0].details.len(), 2);
}

#[tokio::test]
async fn test_periods_restore_queues_the_own_acceptance_again_once() {
    // The request record keeps the wallet's acceptance: restored from a backup, whose
    // outbox holds nothing sent before, the wallet queues it again, once, and the shop
    // takes the copy as the acceptance it holds (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    let accepted = wallet.accept_subscription(&request).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert!(!queued(&wallet, &accepted.msg_id).await);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_get(b"ro").await.unwrap().is_none());
    assert!(queued(&wallet, &accepted.msg_id).await);
    let after = w.bob.switch().entries_from(&w.alice_id());
    wallet.receive(None).await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), after);
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Accepted
    );
}

#[tokio::test]
async fn test_periods_restore_own_details_wait_while_sends_to_the_payer_are_held() {
    // Alice is blocked when the restored till's re-send runs: neither details goes, and the
    // re-send stays owed; once she is unblocked both go, each once (Paykit spec 7.2, "What a
    // restore loses"; 8.1, Blocked counterparties).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    let (_, _, sent) = details_twice(&w, &mut till, &mut wallet, 1_000).await;
    let backup = till.backup().await.unwrap();
    drop(till);
    let mut till = bob_restored(&w, &backup).await;
    till.block(&w.alice_id()).await.unwrap();
    let before = w.alice.switch().entries_from(&w.bob_id());
    for _ in 0..2 {
        till.receive(None).await.unwrap();
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
    assert!(till.app().protocol_get(b"ro").await.unwrap().is_some());
    for msg_id in &sent {
        assert!(!queued(&till, msg_id).await, "{msg_id}");
    }
    till.unblock(&w.alice_id()).await.unwrap();
    till.receive(None).await.unwrap();
    assert!(till.app().protocol_get(b"ro").await.unwrap().is_none());
    for msg_id in &sent {
        assert!(queued(&till, msg_id).await, "{msg_id}");
    }
    // The restore's block mark of alice asks too, once, when its read shows her unblocked.
    let asks = crate::common::asked(&till, &w.alice_id()).await.len();
    assert_eq!(
        w.alice.switch().entries_from(&w.bob_id()),
        before + 2 + asks
    );
    till.receive(None).await.unwrap();
    assert_eq!(
        w.alice.switch().entries_from(&w.bob_id()),
        before + 2 + asks
    );
}

#[tokio::test]
async fn test_periods_restore_own_details_past_their_replay_window_not_sent_again() {
    // The first details were sent 20 days before the second, and the shop is restored 11
    // days after the second: the first is past its 30 days from its `created_at`, which the
    // client refuses to queue again, nothing told; the second goes, once (Paykit spec 7.2,
    // "What a restore loses").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (_, _, sent) = details_twice(&w, &mut shop, &mut wallet, 20 * DAY_MS).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = bob_restored(&w, &backup).await;
    w.clock.advance_ms(11 * DAY_MS);
    let before = w.alice.switch().entries_from(&w.bob_id());
    let summary = shop.receive(None).await.unwrap();
    assert!(
        !summary
            .report
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::MessageUndelivered { .. })),
        "{:?}",
        summary.report.events
    );
    assert!(shop.notices().await.unwrap().is_empty());
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_none());
    assert!(!queued(&shop, &sent[0]).await);
    assert!(queued(&shop, &sent[1]).await);
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + 1);
    shop.receive(None).await.unwrap();
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + 1);
}

/// Bob's payee request record of `request`.
pub(crate) fn payee_request_at(request: &RequestRef) -> RecordTarget {
    let collection = Collection::PayeeActivity;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    )
}

/// `app` writes `bytes` as the value of the record at `target`.
pub(crate) async fn writes_value(app: &mut Paykit, target: &RecordTarget, bytes: &[u8]) {
    let op = app
        .app_mut()
        .record_op(target, Some(bytes), Vec::new())
        .await
        .unwrap();
    app.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_periods_restore_after_the_payee_cancelled_queues_no_details() {
    // The shop cancelled the subscription after sending January's details: the request
    // record's cancellation is a later stop that covers them, so the restored shop queues
    // the cancellation again and neither details (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (_, sale, sent) = details_twice(&w, &mut shop, &mut wallet, 1_000).await;
    let cancellation = shop.cancel_sent_request(&sale, None).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = bob_restored(&w, &backup).await;
    let before = w.alice.switch().entries_from(&w.bob_id());
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_none());
    assert!(queued(&shop, &cancellation).await);
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + 1);
}

#[tokio::test]
async fn test_periods_restore_after_the_period_is_credited_queues_no_details() {
    // January is credited after its details were sent, its receipt in its period record: a
    // later stop that covers the details. The restored shop queues the receipt again and
    // neither details (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _, sent) = details_twice(&w, &mut shop, &mut wallet, 1_000).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
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
    let period = shop.payee_activity().await.unwrap()[0].periods[0].record;
    let receipt = shop.confirm_payment(&period).await.unwrap().unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = bob_restored(&w, &backup).await;
    let before = w.alice.switch().entries_from(&w.bob_id());
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_none());
    assert!(queued(&shop, &receipt).await);
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + 1);
}

#[tokio::test]
async fn test_periods_restore_own_details_wait_while_their_request_record_doesnt_read() {
    // After the backup the till writes the shop's request record as a value no reader reads:
    // the restored shop can't tell whether a stop covers the details, so they wait, neither
    // sent nor dropped, the re-send still owed. Once the till writes the record back, they
    // go, each once (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _, sent) = details_twice(&w, &mut shop, &mut wallet, 1_000).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    till.receive(None).await.unwrap();
    let target = payee_request_at(&request);
    let good = till.app_mut().read_record(&target).await.unwrap().unwrap();
    let good = good.content.value().unwrap().to_vec();
    let mut value: serde_json::Value = serde_json::from_slice(&good).unwrap();
    value["v"] = json!(99);
    writes_value(&mut till, &target, &serde_json::to_vec(&value).unwrap()).await;
    let mut shop = bob_restored(&w, &backup).await;
    let before = w.alice.switch().entries_from(&w.bob_id());
    for _ in 0..2 {
        shop.receive(None).await.unwrap();
    }
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_some());
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
    writes_value(&mut till, &target, &good).await;
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_none());
    for msg_id in &sent {
        assert!(queued(&shop, msg_id).await, "{msg_id}");
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + 2);
    shop.receive(None).await.unwrap();
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + 2);
}

#[tokio::test]
async fn test_periods_restore_own_details_waiting_on_their_request_record_end_past_their_window() {
    // The request record stays a value no reader reads: the details wait until both are
    // past the 30 days the client would queue them again in, and then the re-send is owed
    // no more, with nothing sent and nothing told (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _, sent) = details_twice(&w, &mut shop, &mut wallet, 1_000).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    till.receive(None).await.unwrap();
    let target = payee_request_at(&request);
    writes_value(
        &mut till,
        &target,
        &serde_json::to_vec(&json!({ "v": 99 })).unwrap(),
    )
    .await;
    let mut shop = bob_restored(&w, &backup).await;
    let before = w.alice.switch().entries_from(&w.bob_id());
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_some());
    w.clock.advance_ms(31 * DAY_MS);
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_none());
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
    assert!(shop.notices().await.unwrap().is_empty());
}

/// `stored`, a stored message of alice's wallet in a record's value, signed again by the
/// wallet as a message of `kind` with an empty body, naming the same `refs`, under a new
/// `msg_id`: a genuine message of that app, as another own app might have recorded it.
fn alice_signs_again(w: &World, stored: &serde_json::Value, kind: &str) -> serde_json::Value {
    use pubky_switch_core::enc::{b64, unb64};
    use pubky_switch_core::message::{Message, Sender};
    let json = unb64(stored["plaintext"]["json"].as_str().unwrap()).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let refs = json["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| <[u8; 32]>::try_from(unb64(r.as_str().unwrap()).unwrap().as_slice()).unwrap())
        .collect();
    let keys = w.alice.identity().app_keys("wallet");
    let message = Message {
        kind: kind.to_owned(),
        msg_id: "0d6f5b9e-3c1a-4e8b-9f2d-7a4c1e5b8d30".into(),
        created_at_ms: w.clock.now_ms(),
        from: Sender {
            identity: w.alice_id(),
            app_id: keys.app_id_bytes(),
        },
        to: w.bob_id(),
        refs,
        body: serde_json::Map::new(),
    };
    let signed = message.sign(&keys).unwrap();
    let mut again = stored.clone();
    again["plaintext"] = json!({
        "sender_app": b64(&signed.sender_app_id()),
        "signature": b64(&signed.signature()),
        "json": b64(signed.json()),
    });
    again
}

/// The shop's subscription, with January's details sent twice, then, after its backup, its
/// request record rewritten by the till with `edit`. Returns the restored shop, received
/// once, and the details' `msg_id`s.
async fn restored_over_an_answer(
    w: &World,
    edit: impl Fn(&mut serde_json::Value),
) -> (Paykit, [String; 2]) {
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _, sent) = details_twice(w, &mut shop, &mut wallet, 1_000).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    till.receive(None).await.unwrap();
    let target = payee_request_at(&request);
    let held = till.app_mut().read_record(&target).await.unwrap().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().unwrap()).unwrap();
    edit(&mut value);
    writes_value(&mut till, &target, &serde_json::to_vec(&value).unwrap()).await;
    let mut shop = bob_restored(w, &backup).await;
    shop.receive(None).await.unwrap();
    (shop, sent)
}

#[tokio::test]
async fn test_periods_restore_after_the_payer_rejected_queues_no_details() {
    // The request record holds alice's rejection in place of her acceptance, as another own
    // app recorded it: a later stop that covers the details, so the restored shop queues
    // neither, and the re-send is owed no more (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let (shop, sent) = restored_over_an_answer(&w, |value| {
        value["response"] =
            alice_signs_again(&w, &value["response"], "paykit.payment_request_rejection");
    })
    .await;
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_none());
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
}

#[tokio::test]
async fn test_periods_restore_after_the_payers_cancellation_queues_no_details() {
    // The request record holds alice's cancellation of the subscription: a later stop that
    // covers the details, so the restored shop queues neither (Paykit spec 7.2, "What a
    // restore loses").
    let w = World::new();
    let (shop, sent) = restored_over_an_answer(&w, |value| {
        value["cancellation"] = alice_signs_again(
            &w,
            &value["response"],
            "paykit.payment_request_cancellation",
        );
    })
    .await;
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_none());
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
}

#[tokio::test]
async fn test_periods_restore_details_waiting_leave_another_own_message_queued_once() {
    // The shop's details wait for their request record, which the till left as a value no
    // reader reads, while its one-time request to alice, unanswered, is found at every
    // receive: it is queued once in all, across three receives.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _, sent) = details_twice(&w, &mut shop, &mut wallet, 1_000).await;
    let mut unexpiring = draft("1", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let one_time = shop.send_request(w.alice_id(), unexpiring).await.unwrap();
    wallet.receive(None).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    till.receive(None).await.unwrap();
    writes_value(
        &mut till,
        &payee_request_at(&request),
        &serde_json::to_vec(&json!({ "v": 99 })).unwrap(),
    )
    .await;
    let mut shop = bob_restored(&w, &backup).await;
    let before = w.alice.switch().entries_from(&w.bob_id());
    for _ in 0..3 {
        shop.receive(None).await.unwrap();
    }
    assert!(queued(&shop, &one_time.msg_id).await);
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
    assert!(shop.app().protocol_get(b"ro").await.unwrap().is_some());
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + 1);
}

#[tokio::test]
async fn test_periods_credit_stays_given_after_a_restore() {
    // A period credited, its receipt in its record: restored from a backup taken before
    // the credit, the shop sends no second receipt, and the payment credits no other
    // occurrence.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
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
    let backup = shop.backup().await.unwrap();
    let period = shop.payee_activity().await.unwrap()[0].periods[0].record;
    let receipt = shop.confirm_payment(&period).await.unwrap().unwrap();
    drop(shop);
    let mut shop = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    shop.receive(None).await.unwrap();
    // The receipt the period record keeps is queued again, the same message.
    assert!(queued(&shop, &receipt).await);
    assert_eq!(
        shop.confirm_payment(&period).await.unwrap(),
        Some(receipt.clone())
    );
    let summary = wallet.receive(None).await.unwrap();
    let receipts = summary
        .report
        .events
        .iter()
        .filter(|e| matches!(e, AppEvent::MessageReceived { kind, .. } if kind == "paykit.payment_receipt"))
        .count();
    assert_eq!(receipts, 1);
}

/// Alice's payer period record of `request`'s `period`.
fn payer_period_at(request: &RequestRef, period: PeriodRef) -> RecordTarget {
    let collection = Collection::PayerActivity;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::period_record(&request.payee, &request.request_hash, period.start_secs()),
    )
}

/// Alice's `phone` reads her period record of `request`'s `period`, adds her proof of
/// its payment, as an own app that paid it writes it, and writes the value at `at`: the
/// period's own record, which then shows it paid, or another record's key.
async fn payer_period_paid(
    w: &World,
    phone: &mut Paykit,
    request: &RequestRef,
    payee_app: [u8; 16],
    period: PeriodRef,
    at: &RecordTarget,
) {
    let held = phone
        .app_mut()
        .read_record(&payer_period_at(request, period))
        .await
        .unwrap()
        .unwrap();
    let mut record = paykit_lib::records::ActivityRecord::read(
        held.content.value().unwrap(),
        &w.alice_id(),
        paykit_lib::records::ActivitySide::Payer,
    )
    .unwrap();
    let body = period_proof(w, payee_app, period).await;
    let plaintext = w.sign_as(
        &w.alice,
        "phone",
        &w.bob,
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );
    record.proof = Some(
        paykit_lib::records::StoredMessage::new(
            w.alice_id(),
            phone.app().cert().signed(),
            plaintext,
            None,
            &w.bob_id(),
        )
        .unwrap(),
    );
    let value = record.to_json().unwrap();
    let op = phone
        .app_mut()
        .record_op(at, Some(&value), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
}

/// Bob's shop's later details for January of the request `hash`, entered at alice's switch
/// for `targets`. Returns their `msg_id`.
fn inject_later_details(w: &World, targets: &[[u8; 16]], hash: [u8; 32]) -> String {
    w.clock.advance_ms(1_000);
    let body = details(january(), "lnbc1later", w.clock.now_ms() + DAY_MS)
        .to_body()
        .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        targets,
        "paykit.payment_period_details",
        vec![hash],
        body,
    )
}

#[tokio::test]
async fn test_periods_details_for_a_period_the_payer_shows_paid_taken_at_receive() {
    // The phone paid January: alice's period record holds its proof. Bob's later details
    // for January reach the wallet, which stores none for a period shown paid (Paykit spec
    // 8.2): the receive step reads the period record's paid state.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1a", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let january_record = payer_period(&request);
    payer_period_paid(
        &w,
        &mut phone,
        &request,
        shop.app_id(),
        january(),
        &january_record,
    )
    .await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.unwrap().periods[0].paid);
    inject_later_details(&w, &[wallet.app_id()], request.request_hash);
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.periods[0].details.len(), 1, "{view:?}");
}

#[tokio::test]
async fn test_periods_details_for_a_period_the_payer_shows_paid_taken_at_the_later_write() {
    // Bob's later details for January reach the wallet, whose record write of them is
    // refused for good twice, so they stay a stored entry. The phone then pays January:
    // alice's period record holds its proof. The stored entry's later write takes the
    // details with no effect, as the period shows paid (Paykit spec 8.2).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1a", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    inject_later_details(&w, &[wallet.app_id()], request.request_hash);
    for _ in 0..2 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::PermissionMissing,
                current: serde_json::Value::Null,
            });
    }
    wallet.receive(None).await.unwrap();
    assert!(!wallet.app_mut().parked_entries().await.unwrap().is_empty());
    let january_record = payer_period(&request);
    payer_period_paid(
        &w,
        &mut phone,
        &request,
        shop.app_id(),
        january(),
        &january_record,
    )
    .await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert!(view.periods[0].paid, "{view:?}");
    assert_eq!(view.periods[0].details.len(), 1, "{view:?}");
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_periods_details_over_a_period_record_that_doesnt_bind_kept_not_dropped() {
    // The phone writes, at January's period record, February's record with a proof in it:
    // a value that reads but doesn't bind to that key, which the reader doesn't act on
    // (Paykit wire 5.3). Bob's January details then reach the wallet. Its paid state can't
    // be told from that value, so the details are neither taken with no effect nor written
    // over it: they are kept, and given up with the host's notice past their 30 days.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(month(1), "lnbc1feb", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let january_record = payer_period(&request);
    payer_period_paid(
        &w,
        &mut phone,
        &request,
        shop.app_id(),
        month(1),
        &january_record,
    )
    .await;
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&january_record).unwrap();
    let phones = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let msg_id = inject_later_details(&w, &[wallet.app_id()], request.request_hash);
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    wallet.receive(None).await.unwrap();
    let after = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(after.version, phones.version);
    assert_eq!(after.content, phones.content);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    wallet.receive(None).await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert!(
        notices
            .iter()
            .any(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                && n.msg_id.as_deref() == Some(msg_id.as_str())),
        "{notices:?}"
    );
    let after = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(after.content, phones.content);
}

#[tokio::test]
async fn test_periods_proof_over_a_payee_period_record_that_doesnt_bind_kept_not_written() {
    // Bob's till writes, at the shop's January period record, February's record: a value
    // that reads but doesn't bind to that key (Paykit wire 5.3). Alice's proof of January
    // then reaches the shop. It is never written over that value: it is kept as a reply
    // before its record, and given up with the host's notice past its 30 days.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(month(1), "lnbc1feb", now + DAY_MS))
        .await
        .unwrap();
    till.receive(None).await.unwrap();
    let february = till
        .app_mut()
        .read_record(&payee_period_at(&request, month(1)))
        .await
        .unwrap()
        .unwrap();
    let january_record = payee_period(&request);
    let op = till
        .app_mut()
        .record_op(
            &january_record,
            Some(february.content.value().unwrap()),
            Vec::new(),
        )
        .await
        .unwrap();
    till.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let key = shop.app().record_key(&january_record).unwrap();
    let tills = shop.app().shared_record(&key).await.unwrap().unwrap();
    let body = period_proof(&w, shop.app_id(), january()).await;
    let msg_id = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );
    let summary = shop.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_proof"));
    shop.receive(None).await.unwrap();
    let after = shop.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(after.version, tills.version);
    assert_eq!(after.content, tills.content);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    shop.receive(None).await.unwrap();
    let notices = shop.notices().await.unwrap();
    assert!(
        notices
            .iter()
            .any(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                && n.msg_id.as_deref() == Some(msg_id.as_str())),
        "{notices:?}"
    );
}

#[tokio::test]
async fn test_periods_stored_proof_waits_for_its_details_after_the_record_is_deleted() {
    // The shop holds January's details; a proof naming them comes and its record write is
    // refused for good twice, so it stays a stored entry. The till then deletes the period
    // record (Paykit spec 6 allows deleting an activity record). The later write never
    // opens a new period record holding the proof without the details it names: the entry
    // waits, and past its window it is given up, the host told once.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1invoice", now + DAY_MS))
        .await
        .unwrap();
    let target = payee_period(&request);
    let key = shop.app().record_key(&target).unwrap();
    let value = shop.app().shared_record(&key).await.unwrap().unwrap();
    let details_hash = paykit_lib::records::ActivityRecord::read(
        value.content.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap()
    .period_details
    .unwrap()
    .hash();
    let body = period_proof(&w, shop.app_id(), january()).await;
    for _ in 0..2 {
        w.bob
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::PermissionMissing,
                current: serde_json::Value::Null,
            });
    }
    let msg_id = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, details_hash],
        body,
    );
    shop.receive(None).await.unwrap();
    assert!(!shop.app_mut().parked_entries().await.unwrap().is_empty());
    till.receive(None).await.unwrap();
    let op = till
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    till.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        matches!(held.content, RecordContent::Deleted),
        "{:?}",
        held.content
    );
    assert!(!shop.app_mut().parked_entries().await.unwrap().is_empty());
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        matches!(held.content, RecordContent::Deleted),
        "{:?}",
        held.content
    );
    let given_up = shop
        .notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|n| {
            n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                && n.msg_id.as_deref() == Some(msg_id.as_str())
        })
        .count();
    assert_eq!(given_up, 1);
}

#[tokio::test]
async fn test_periods_send_details_window_judged_on_the_time_read_again() {
    // The shop last verified the service's time when it read the acceptance, within
    // January's deadline. Two days on, with no read since, the records read again verify
    // the time now: January's deadline has passed, and its details are refused.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let draft = monthly(now, None).with_period_deadline_s(86_400);
    let (_, sale) = subscribed_for(&w, &mut shop, &mut wallet, draft, 0).await;
    let stale = shop.app().highest_service_time().unwrap();
    assert!(stale < january().starts_at + DAY_MS);
    w.clock.advance_ms(2 * DAY_MS);
    let refused = shop
        .send_period_details(
            &sale,
            details(january(), "lnbc1p", w.clock.now_ms() + DAY_MS),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, PaykitError::Lib(paykit_lib::Error::Validation(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_periods_send_details_after_another_app_cancelled_unseen_refused() {
    // Bob's till cancels the subscription; the shop hasn't read that write. Its details are
    // judged on the request record read again, which shows the cancellation: refused, and
    // nothing reaches the wallet.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    till.receive(None).await.unwrap();
    let tills = till.payee_activity().await.unwrap()[0].record;
    till.cancel_sent_request(&tills, None).await.unwrap();
    let refused = shop
        .send_period_details(
            &sale,
            details(january(), "lnbc1p", w.clock.now_ms() + DAY_MS),
        )
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Cancelled);
    let summary = wallet.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_period_details"));
    assert!(wallet.request(&request).await.unwrap().periods.is_empty());
}

#[tokio::test]
async fn test_periods_credit_not_now_while_the_request_record_is_sealed_then_given() {
    // The shop holds January's proof in its period record. Bob's till writes the shop's
    // request record again at a data epoch the shop holds no key of yet: the credit is
    // refused for now, not as a record not found, and given once the key comes.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
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
    let period = shop.payee_activity().await.unwrap()[0].periods[0].record;
    till.receive(None).await.unwrap();
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bob_bundle(&w, till.app_id(), &till.app().cert().clone());
    till.receive(None).await.unwrap();
    let collection = Collection::PayeeActivity;
    let request_record = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    );
    rewrites(&mut till, &request_record).await;
    shop.receive(None).await.unwrap();
    let refused = shop.confirm_payment(&period).await.unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NotNow(_)),
        "{refused:?}"
    );
    let refused = shop.verify_payment(&period).await.unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NotNow(_)),
        "{refused:?}"
    );
    store_bob_bundle(&w, shop.app_id(), &shop.app().cert().clone());
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.confirm_payment(&period).await.unwrap().is_some());
}

#[tokio::test]
async fn test_periods_details_from_a_flagged_payee_over_a_period_record_that_doesnt_bind_kept() {
    // The phone writes, at January's period record, February's record with a proof in it:
    // a value that reads but doesn't bind to that key (Paykit wire 5.3). Bob is flagged
    // since; his later January details reach the contacts wallet. They update a
    // subscription the wallet holds, no new state, so the spam rule doesn't gate them: they
    // are kept, never written over that value, and given up with the host's notice past
    // their 30 days.
    let w = World::new();
    let mut shop = w.shop().await;
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut wallet = contacts_wallet(&w, &lists).await;
    let mut phone = w.wallet("phone").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(month(1), "lnbc1feb", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let january_record = payer_period(&request);
    payer_period_paid(
        &w,
        &mut phone,
        &request,
        shop.app_id(),
        month(1),
        &january_record,
    )
    .await;
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&january_record).unwrap();
    let phones = wallet.app().shared_record(&key).await.unwrap().unwrap();
    lists.flag(&w.bob_id());
    let msg_id = inject_later_details(&w, &[wallet.app_id()], request.request_hash);
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_period_details"));
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 0);
    let after = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(after.version, phones.version);
    assert_eq!(after.content, phones.content);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    wallet.receive(None).await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert!(
        notices
            .iter()
            .any(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                && n.msg_id.as_deref() == Some(msg_id.as_str())),
        "{notices:?}"
    );
    let after = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(after.content, phones.content);
}

#[tokio::test]
async fn test_periods_proof_from_a_flagged_payer_over_a_payee_period_record_that_doesnt_bind_kept()
{
    // Bob's till holds `contacts`. His shop writes, at the till's January period record,
    // February's record: a value that reads but doesn't bind to that key (Paykit wire 5.3).
    // Alice is flagged since, and her proof of a real payment of January reaches the till.
    // It updates a subscription the till holds, no new state, so the spam rule doesn't gate
    // it: it is kept, never written over that value, and given up with the host's notice
    // past its 30 days.
    let w = World::new();
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut till = w
        .join_with(
            &w.bob,
            "till",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            Arc::clone(&lists) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    till.receive(None).await.unwrap();
    let request = till
        .send_request(w.alice_id(), monthly(w.clock.now_ms(), None))
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    till.receive(None).await.unwrap();
    let sale = till.payee_activity().await.unwrap()[0].record;
    let now = w.clock.now_ms();
    till.send_period_details(&sale, details(month(1), "lnbc1feb", now + DAY_MS))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let february = shop
        .app_mut()
        .read_record(&payee_period_at(&request, month(1)))
        .await
        .unwrap()
        .unwrap();
    let january_record = payee_period(&request);
    let op = shop
        .app_mut()
        .record_op(
            &january_record,
            Some(february.content.value().unwrap()),
            Vec::new(),
        )
        .await
        .unwrap();
    shop.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    till.receive(None).await.unwrap();
    let key = till.app().record_key(&january_record).unwrap();
    let shops = till.app().shared_record(&key).await.unwrap().unwrap();
    lists.flag(&w.alice_id());
    let body = period_proof(&w, till.app_id(), january()).await;
    let msg_id = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[till.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );
    let summary = till.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_proof"));
    till.receive(None).await.unwrap();
    assert_eq!(till.spam_count(&w.alice_id()).await.unwrap(), 0);
    let after = till.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(after.version, shops.version);
    assert_eq!(after.content, shops.content);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    till.receive(None).await.unwrap();
    let notices = till.notices().await.unwrap();
    assert!(
        notices
            .iter()
            .any(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                && n.msg_id.as_deref() == Some(msg_id.as_str())),
        "{notices:?}"
    );
}

/// After a `missed` item, the next receive's listing fails at Paykit's own listing: the
/// client's listing right after the item, of the wallet's three collections, goes through.
fn missed_with_paykits_listing_failing(w: &World) {
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "records?collection".into(),
            after: 3,
        });
}

#[tokio::test]
async fn test_periods_details_taken_with_no_effect_held_while_the_records_are_unlisted() {
    // After a `missed` item whose listing fails, the payer's copy may lack records: details
    // its copy would take with no effect, for a request it hasn't accepted here, are held,
    // and taken once the records are listed.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    missed_with_paykits_listing_failing(&w);
    inject_details(&w, &[wallet.app_id()], request.request_hash, january());
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.records_unlisted.is_some(), "{summary:?}");
    assert!(
        !received(&summary, "paykit.payment_period_details"),
        "{summary:?}"
    );
    // The next read takes the details while the records are still unlisted: held, and the
    // listing at its end succeeds; the read after that takes them with no effect.
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.records_unlisted.is_none(), "{summary:?}");
    assert!(
        !received(&summary, "paykit.payment_period_details"),
        "{summary:?}"
    );
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        received(&summary, "paykit.payment_period_details"),
        "{summary:?}"
    );
    assert!(wallet.request(&request).await.unwrap().periods.is_empty());
}

#[tokio::test]
async fn test_periods_kept_details_wait_while_the_records_are_unlisted_then_stored() {
    // Bob's details reach the wallet before his request, and are kept. The phone takes the
    // request; after a `missed` item the wallet's client lists it, unaccepted, but Paykit's
    // own listing fails. The kept details, which that record would take with no effect,
    // wait while the copy may lack records. The phone then accepts: once the records are
    // listed the details are stored in the period record.
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (plaintext, hash) = signed_request(&w);
    inject_details(&w, &[wallet.app_id()], hash, january());
    wallet.receive(None).await.unwrap();
    w.deliver(&w.bob, "shop", &w.alice, &[phone.app_id()], &plaintext);
    phone.receive(None).await.unwrap();
    let request = RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    missed_with_paykits_listing_failing(&w);
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.records_unlisted.is_some(), "{summary:?}");
    phone.accept_subscription(&request).await.unwrap();
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    // The request stays held after the `missed` item until the user confirms (Paykit spec
    // 7.2); its period record holds the details all the same.
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.periods.len(), 1, "{view:?}");
    assert_eq!(view.periods[0].details.len(), 1, "{view:?}");
}
#[tokio::test]
async fn test_periods_proof_from_a_flagged_third_party_naming_a_held_request_only_counted() {
    // Carol, flagged, isn't the counterparty of alice's subscription at bob's till, yet she
    // sends a proof naming its request and period details the till doesn't hold. The spam
    // rule gates a message from a sender that is no counterparty of the record it names: it
    // is only counted, and nothing of hers is kept.
    let w = World::new();
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut till = w
        .join_with(
            &w.bob,
            "till",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            Arc::clone(&lists) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let _carol = w
        .join(&w.carol, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    till.receive(None).await.unwrap();
    let request = till
        .send_request(w.alice_id(), monthly(w.clock.now_ms(), None))
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    till.receive(None).await.unwrap();
    lists.flag(&w.carol_id());
    let body = period_proof(&w, till.app_id(), january()).await;
    w.inject(
        &w.carol,
        "wallet",
        &w.bob,
        &[till.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, [1; 32]],
        body,
    );
    till.receive(None).await.unwrap();
    till.receive(None).await.unwrap();
    let kept = till.app().protocol_scan(b"pr/").await.unwrap();
    assert!(kept.is_empty(), "{} kept", kept.len());
    assert_eq!(till.spam_count(&w.carol_id()).await.unwrap(), 1);
}

#[tokio::test]
async fn test_periods_proof_from_a_third_party_never_opens_the_period_record() {
    // Carol, admitted and not flagged, isn't the counterparty of alice's subscription at
    // bob's shop, yet she sends a proof of January naming its request. It opens no period
    // record under her as counterparty: it is kept as a reply before its record, never
    // placed. Alice's own proof of January then goes into the period record.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let _carol = w
        .join(&w.carol, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
    w.inject(
        &w.carol,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        body.clone(),
    );
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap()[0].periods.is_empty());
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
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert_eq!(sale.periods.len(), 1, "{sale:?}");
    assert!(sale.periods[0].paid, "{sale:?}");
}

#[tokio::test]
async fn test_periods_proof_over_a_payee_request_record_that_never_opens_kept_then_given_up() {
    // Bob's till writes the shop's request record as a value sealed under a key that isn't
    // the epoch's: the shop holds the key, so it never opens. Alice's proof of January then
    // reaches the shop: whether she is the record's counterparty can't be told, so it is kept
    // as a reply before its record at once, under the spam rule, never written over the
    // value (Paykit spec 8.1, "Replies before their record"). Past 30 days from its
    // acceptance it is given up with the host's notice.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    till.receive(None).await.unwrap();
    // A new data epoch whose key only the till holds yet.
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bob_bundle(&w, till.app_id(), &till.app().cert().clone());
    till.receive(None).await.unwrap();
    let collection = Collection::PayeeActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    );
    let value = till
        .app_mut()
        .read_record(&target)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    let (conditions, kind) = till
        .app_mut()
        .record_op(&target, Some(&value), Vec::new())
        .await
        .unwrap();
    let pubky_switch_core::ops::OpKind::RecordPut(mut put) = kind else {
        panic!("a record op");
    };
    let epoch = w.bob.switch().identity_doc().unwrap().data_epoch();
    put.value_sealed = pubky_switch_core::sealed::SealedValue::seal(
        &[3; 32],
        &w.bob_id(),
        epoch,
        &put.policy.read_scope,
        &put.key,
        &value,
    )
    .unwrap();
    till.app_mut()
        .submit_batch(
            OpBatch::new().entry((conditions, pubky_switch_core::ops::OpKind::RecordPut(put))),
        )
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    // The key comes: the value doesn't open with it, for good.
    store_bob_bundle(&w, shop.app_id(), &shop.app().cert().clone());
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let key = shop.app().record_key(&target).unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        matches!(
            held.content,
            RecordContent::Unreadable(Unopened::DoesNotOpen)
        ),
        "{:?}",
        held.content
    );
    let body = period_proof(&w, shop.app_id(), january()).await;
    let msg_id = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );
    let summary = shop.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_proof"), "{summary:?}");
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    for _ in 0..3 {
        shop.receive(None).await.unwrap();
    }
    assert!(shop.app().protocol_scan(b"pr/").await.unwrap().is_empty());
    let notices = shop.notices().await.unwrap();
    assert!(
        notices
            .iter()
            .any(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp
                && n.msg_id.as_deref() == Some(msg_id.as_str())),
        "{notices:?}"
    );
}

/// Alice's payer record of `request`.
fn payer_request_at(request: &RequestRef) -> RecordTarget {
    let collection = Collection::PayerActivity;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    )
}

/// How alice's wallet holds her record of bob's subscription while his details come.
#[derive(Clone, Copy, Debug)]
enum RecordWhile {
    /// Deleted by her phone: the wallet holds no record of the request.
    Gone,
    /// Written by her phone as a value that doesn't read, which her wallet, restored from
    /// a backup, holds as the switch does.
    Unreadable,
}

/// Bob's shop sends January's details while alice's wallet holds no record of the
/// subscription that binds (`record_while`), so they are kept before their record, and then `more`
/// details of bob's naming a period that isn't the request's are kept after them. Past the
/// 64 kept per sender the earliest, the shop's, is dropped and marked, and the mark isn't
/// due. Once the phone writes the record back, the wallet asks bob to send again, once,
/// from the mark's earliest time, after he is unblocked when `blocked`, and the copy the
/// shop sends again is stored; with nothing dropped, nothing is asked and the kept details
/// are stored. With `february`, the wallet holds February's period record of bob's
/// throughout, which reads and makes no mark due: only a request's record does (Paykit
/// spec 8.1, "Replies before their record"; Paykit spec 9).
async fn details_dropped_for_the_bounds(
    record_while: RecordWhile,
    more: usize,
    blocked: bool,
    february: bool,
) {
    let w = World::new();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    // February's details, stored: a period record of bob's the wallet holds, which reads.
    let mut february_details = None;
    if february {
        let sent = shop
            .send_period_details(
                &sale,
                details(month(1), "lnbc1feb", w.clock.now_ms() + DAY_MS),
            )
            .await
            .unwrap();
        wallet.receive(None).await.unwrap();
        february_details = Some(sent.msg_id);
    }
    phone.receive(None).await.unwrap();
    let target = payer_request_at(&request);
    let held = phone.app_mut().read_record(&target).await.unwrap().unwrap();
    let good = held.content.value().unwrap().to_vec();
    match record_while {
        RecordWhile::Gone => {
            let op = phone
                .app_mut()
                .record_op(&target, None, Vec::new())
                .await
                .unwrap();
            phone
                .app_mut()
                .submit_batch(OpBatch::new().entry(op))
                .await
                .unwrap();
        }
        RecordWhile::Unreadable => {
            // A value that doesn't read, which the wallet's copy, holding the record's
            // value, refuses as a write over it: the wallet is restored from a backup
            // onto a new store, whose copy holds the record as the switch does.
            let backup = wallet.backup().await.unwrap();
            let mut value: serde_json::Value = serde_json::from_slice(&good).unwrap();
            value["v"] = json!(99);
            writes_value(&mut phone, &target, &serde_json::to_vec(&value).unwrap()).await;
            drop(wallet);
            wallet = Paykit::restore(
                w.env(&w.alice, store()),
                PaykitConfig::default(),
                &backup,
                Arc::clone(&w.rail) as Arc<dyn Rail>,
                w.timer(),
            )
            .await
            .unwrap();
        }
    }
    wallet.receive(None).await.unwrap();
    let first_at = w.clock.now_ms();
    let first = shop
        .send_period_details(&sale, january_details(first_at + DAY_MS))
        .await
        .unwrap();
    w.clock.advance_ms(1_000);
    let mut off = january();
    off.starts_at += 1;
    for _ in 0..more {
        inject_details(&w, &[wallet.app_id()], request.request_hash, off);
    }
    wallet.receive(None).await.unwrap();
    let dropped = more + 1 > paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let marks: Vec<serde_json::Value> = wallet
        .app()
        .protocol_scan(b"pd/")
        .await
        .unwrap()
        .into_iter()
        .map(|(_, value)| serde_json::from_slice::<serde_json::Value>(&value).unwrap()["d"].clone())
        .collect();
    assert_eq!(
        marks.len(),
        usize::from(dropped),
        "{record_while:?}: {marks:?}"
    );
    if dropped {
        assert_eq!(marks[0]["due"], json!(false), "{record_while:?}");
        assert_eq!(marks[0]["earliest_ms"], json!(first_at), "{record_while:?}");
    }
    // No record of the request reads yet, a period record of bob's included: nothing is
    // asked.
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    if blocked {
        wallet.block(&w.bob_id()).await.unwrap();
    }
    match record_while {
        RecordWhile::Gone => {
            let op = phone
                .app_mut()
                .record_op(&target, Some(&good), Vec::new())
                .await
                .unwrap();
            phone
                .app_mut()
                .submit_batch(OpBatch::new().entry(op))
                .await
                .unwrap();
        }
        RecordWhile::Unreadable => {
            phone.receive(None).await.unwrap();
            writes_value(&mut phone, &target, &good).await;
        }
    }
    let entries = w.bob.switch().entries_from(&w.alice_id());
    let mut summary = wallet.receive(None).await.unwrap();
    if blocked {
        assert_eq!(summary.resends, 0, "{record_while:?}");
        summary = wallet.receive(None).await.unwrap();
        assert_eq!(summary.resends, 0, "{record_while:?}");
        assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
        wallet.unblock(&w.bob_id()).await.unwrap();
        summary = wallet.receive(None).await.unwrap();
    }
    assert_eq!(summary.resends, u64::from(dropped), "{record_while:?}");
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    // Once: a later receive asks nothing more.
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    let view = wallet.request(&request).await.unwrap();
    let mut stored: Vec<&str> = view
        .periods
        .iter()
        .flat_map(|p| p.details.iter().map(|d| d.msg_id.as_str()))
        .collect();
    stored.sort_unstable();
    let mut expected: Vec<&str> = std::iter::once(first.msg_id.as_str())
        .chain(february_details.as_deref())
        .collect();
    expected.sort_unstable();
    assert_eq!(stored, expected, "{record_while:?}: {view:?}");
}

#[tokio::test]
async fn test_periods_details_dropped_for_the_bounds_before_their_record_asked_again() {
    // The driver's future goes on the heap: it drives four apps through a restore.
    Box::pin(details_dropped_for_the_bounds(
        RecordWhile::Gone,
        64,
        false,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_periods_details_dropped_for_the_bounds_over_an_unreadable_record_asked_again() {
    Box::pin(details_dropped_for_the_bounds(
        RecordWhile::Unreadable,
        64,
        false,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_periods_details_within_the_bounds_before_their_record_none_dropped() {
    Box::pin(details_dropped_for_the_bounds(
        RecordWhile::Gone,
        63,
        false,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_periods_details_within_the_bounds_over_an_unreadable_record_none_dropped() {
    Box::pin(details_dropped_for_the_bounds(
        RecordWhile::Unreadable,
        63,
        false,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_periods_details_dropped_for_the_bounds_of_a_blocked_payee_asked_once_unblocked() {
    Box::pin(details_dropped_for_the_bounds(
        RecordWhile::Unreadable,
        64,
        true,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_periods_details_dropped_for_the_bounds_not_asked_on_a_period_record_alone() {
    // The wallet holds February's period record of bob's, which reads, while its record of
    // the request doesn't: only a request's record makes the mark due, so nothing is asked
    // until that record reads.
    Box::pin(details_dropped_for_the_bounds(
        RecordWhile::Gone,
        64,
        false,
        true,
    ))
    .await;
}

// A record paused as divergent beside subscriptions and their Billing Periods (Paykit spec
// 6): nothing is accepted, sent, written or lost past the pause.

/// Bob's shop on `base`.
async fn shop_on(w: &World, base: Arc<dyn pubky_switch_client::store::StateStore>) -> Paykit {
    w.join_with(
        &w.bob,
        "shop",
        AppScopes::new().request().build().unwrap(),
        PaykitConfig::default(),
        base,
    )
    .await
}

/// Whether `error` is a divergent refusal naming `record`.
fn divergent_refusal(error: &PaykitError, record: &[u8; 32]) -> bool {
    matches!(error.refusal(), Some(Refusal::Divergent { records })
        if records.iter().any(|r| r.record == *record))
}

#[tokio::test]
async fn test_periods_subscription_accepted_by_hand_only_once_its_paused_record_is_confirmed() {
    // Alice's request record of bob's subscription is paused as divergent: the payee may
    // have cancelled it. Her acceptance by hand is refused naming the record, nothing
    // recorded, until she confirms it.
    let w = World::new();
    let base = store();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let request = requested(&w, &mut shop, &mut wallet).await;
    let record = wallet.request(&request).await.unwrap().record;
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    let refused = wallet.accept_subscription(&request).await.unwrap_err();
    assert!(divergent_refusal(&refused, &record), "{refused:?}");
    assert_ne!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Subscribed
    );
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Subscribed
    );
}

#[tokio::test]
async fn test_periods_details_sent_only_once_a_paused_record_is_confirmed() {
    // The shop's record of the subscription is paused as divergent: its period details, an
    // own write of another kind than a stop, are refused naming the record, nothing signed
    // or sent, until the user confirms it.
    let w = World::new();
    let base = store();
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let mut wallet = w.wallet("wallet").await;
    let (_, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    // Divergent at the switch, as a conflicting upload leaves it: a fresh read shows it so.
    w.bob
        .switch()
        .make_divergent([0x5a; 16], sale, [1; 32])
        .unwrap();
    shop.receive(None).await.unwrap();
    let now = w.clock.now_ms();
    let refused = shop
        .send_period_details(&sale, details(january(), "lnbc1first", now + DAY_MS))
        .await
        .unwrap_err();
    assert!(divergent_refusal(&refused, &sale), "{refused:?}");
    assert!(shop.payee_activity().await.unwrap()[0].periods.is_empty());
    crate::divergent::confirm(&mut shop, &sale).await.unwrap();
    shop.send_period_details(&sale, details(january(), "lnbc1first", now + DAY_MS))
        .await
        .unwrap();
    assert_eq!(shop.payee_activity().await.unwrap()[0].periods.len(), 1);
}

#[tokio::test]
async fn test_periods_details_over_a_paused_period_record_held_then_placed_once_confirmed() {
    // Alice's period record of January holds bob's first details and is then paused as
    // divergent. Bob's second details are neither written over it nor lost: their entry is
    // held, and placed beside the first once the user confirms the record.
    let w = World::new();
    let base = store();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1first", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let period = wallet.app().record_key(&payer_period(&request)).unwrap();
    async fn count(wallet: &Paykit, request: &RequestRef) -> usize {
        wallet.request(request).await.unwrap().periods[0]
            .details
            .len()
    }
    assert_eq!(count(&wallet, &request).await, 1);
    crate::divergent::set_divergent(&base, &wallet, &period, true);
    w.clock.advance_ms(1_000);
    shop.send_period_details(&sale, details(january(), "lnbc1second", now + 2 * DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(count(&wallet, &request).await, 1);
    assert!(wallet.app().protocol_scan(b"dw/").await.unwrap().is_empty());
    crate::divergent::confirm(&mut wallet, &period)
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(count(&wallet, &request).await, 2);
}

#[tokio::test]
async fn test_periods_proof_over_a_paused_period_record_held_then_placed_once_confirmed() {
    // The shop's period record of January holds its details and is then paused as
    // divergent. Alice's proof of January is neither written over it nor lost, and no
    // kept write names a record it can't: its entry is held, and placed once the user
    // confirms the record.
    let w = World::new();
    let base = store();
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1invoice", now + DAY_MS))
        .await
        .unwrap();
    let period = shop.payee_activity().await.unwrap()[0].periods[0].record;
    crate::divergent::set_divergent(&base, &shop, &period, true);
    let body = period_proof(&w, shop.app_id(), january()).await;
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
    assert!(!shop.payee_activity().await.unwrap()[0].periods[0].paid);
    assert!(shop.app().protocol_scan(b"dw/").await.unwrap().is_empty());
    crate::divergent::confirm(&mut shop, &period).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap()[0].periods[0].paid);
}
