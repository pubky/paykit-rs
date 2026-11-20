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
pub(crate) fn january() -> PeriodRef {
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
pub(crate) fn signed_request(
    w: &World,
) -> (pubky_switch_core::message::MessagePlaintext, [u8; 32]) {
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
pub(crate) fn inject_details(w: &World, targets: &[[u8; 16]], hash: [u8; 32], period: PeriodRef) {
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
    // The wallet is narrowed to `paykit_contacts`; meanwhile bob's request comes and the phone
    // accepts it. The wallet is renewed to `paykit_pay` and `paykit_request` with no bundle, the
    // `paykit_request` key missing, so its records can't be listed, and bob's details come. The
    // request record's absence from the wallet's copy decides nothing: the entry is held, not kept
    // as details before their record. Once the keys come and the records are listed, the details go
    // into the period record.
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
pub(crate) async fn period_proof(
    w: &World,
    payee_app: [u8; 16],
    period: PeriodRef,
) -> serde_json::Map<String, serde_json::Value> {
    period_proof_for_attempt(w, payee_app, period, AttemptId::from_bytes([9; 16])).await
}

async fn period_proof_for_attempt(
    w: &World,
    payee_app: [u8; 16],
    period: PeriodRef,
    attempt: AttemptId,
) -> serde_json::Map<String, serde_json::Value> {
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
    assert_eq!(held.payee, w.bob_id());
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
    // Alice's wallet sends bob a monthly request, which bob's wallet accepts; the seller wallet's
    // record is written while alice's wallet, holding `paykit_pay` only, can't read it. The wallet
    // is renewed to read `paykit_request` too, with no bundle, and bob's proof of January comes.
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
    let mut seller_wallet = w
        .join(
            &w.alice,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = seller_wallet
        .send_request(w.bob_id(), monthly(w.clock.now_ms(), None))
        .await
        .unwrap()
        .request
        .unwrap();
    payer.receive(None).await.unwrap();
    payer.accept_subscription(&request).await.unwrap();
    drop(seller_wallet);
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
    let seller_wallet_id = w.alice.identity().app_keys("seller-wallet").app_id_bytes();
    let body = period_proof(&w, seller_wallet_id, january()).await;
    let proof = w.inject(
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
    crate::common::receive_message(&mut wallet, &proof).await;
    assert!(!wallet.receive(None).await.unwrap().applied.contains(&proof));
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
    crate::common::restore_with_unknown_cursor(&mut app).await;
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

/// Alice's wallet holding `paykit_pay` and `paykit_manage_contacts` on `lists`, inviting bob.
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
    crate::common::restore_with_unknown_cursor(&mut app).await;
    app.resume_payments().await.unwrap();
    app.receive(None).await.unwrap();
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
    let refused = app.accept_subscription(&request).await.unwrap_err();
    assert!(matches!(
        refusal(&refused),
        Refusal::HeldAfterRestore { .. }
    ));
    let options = paykit_sdk::AcceptOptions::default()
        .with_possibly_cancelled_confirmed(crate::common::held(&app, &request).await);
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
    // Bob's wallet writes the shop's period record again at a data epoch the shop holds no
    // key of yet: the shop's next details are refused for now, never written over a value
    // it can't see.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1a", now + DAY_MS))
        .await
        .unwrap();
    seller_wallet.receive(None).await.unwrap();
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bob_bundle(
        &w,
        seller_wallet.app_id(),
        &seller_wallet.app().cert().clone(),
    );
    seller_wallet.receive(None).await.unwrap();
    rewrites(&mut seller_wallet, &payee_period(&request)).await;
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
    // A later period with per-period endpoints is credited against its named details.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let draft = monthly(w.clock.now_ms(), None).with_per_period_endpoints();
    let (request, sale) = subscribed_for(&w, &mut shop, &mut wallet, draft, 40).await;
    let period = month(1);
    shop.send_period_details(
        &sale,
        details(period, "lnbc1details", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    let held = shop
        .app_mut()
        .read_record(&payee_period_at(&request, period))
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    let named =
        paykit_lib::records::StoredMessage::read(&value["period_details"], &w.alice_id()).unwrap();
    let body = proof_to(&w, shop.app_id(), period, "lnbc1details", 5).await;
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
async fn test_periods_per_period_endpoints_details_invalid_against_the_request_credit_nothing() {
    // With per-period endpoints and no conversion terms, the payee's February record holds
    // details with rates and a destination, as a record written by another app would: they
    // are invalid against the request, so a payment of February to their destination, its
    // proof naming them, is credited by none, with no receipt stored or sent.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let now = w.clock.now_ms();
    let draft = monthly(now, None).with_per_period_endpoints();
    let (request, sale) = subscribed_for(&w, &mut shop, &mut wallet, draft, 40).await;
    shop.send_period_details(
        &sale,
        details(month(1), "lnbc1february", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    let target = payee_period_at(&request, month(1));
    let held = shop.app_mut().read_record(&target).await.unwrap().unwrap();
    let mut record = paykit_lib::records::ActivityRecord::read(
        held.content.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap();
    w.clock.advance_ms(1);
    let mut body = details(month(1), "lnbc1rated", w.clock.now_ms() + DAY_MS)
        .to_body()
        .unwrap();
    body.insert(
        "rates".into(),
        json!([{ "asset": "usd", "value": "100000" }]),
    );
    let plaintext = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_period_details",
        vec![request.request_hash],
        body,
    );
    let rated = paykit_lib::records::StoredMessage::new(
        w.bob_id(),
        shop.app().cert().signed(),
        plaintext,
        None,
        &w.alice_id(),
    )
    .unwrap();
    let valid = record.period_details.replace(rated.clone()).unwrap();
    record.earlier_details.push(valid);
    record.msg_ids.push(rated.message().msg_id.clone());
    let op = shop
        .app_mut()
        .record_op(&target, Some(&record.to_json().unwrap()), Vec::new())
        .await
        .unwrap();
    shop.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    let body = proof_to(&w, shop.app_id(), month(1), "lnbc1rated", 9).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, rated.hash()],
        body,
    );
    shop.receive(None).await.unwrap();
    let february = |periods: Vec<paykit_sdk::PeriodView>| {
        periods
            .into_iter()
            .find(|p| p.billing_period == month(1))
            .unwrap()
    };
    let period = february(shop.payee_activity().await.unwrap()[0].periods.clone());
    // Only the details valid against the request show.
    assert_eq!(period.details.len(), 1);
    assert!(period.paid);
    assert!(shop.verify_payment(&period.record).await.unwrap().is_none());
    assert!(shop
        .confirm_payment(&period.record)
        .await
        .unwrap()
        .is_none());
    let period = february(shop.payee_activity().await.unwrap()[0].periods.clone());
    assert!(!period.credited && period.receipt.is_none(), "{period:?}");
    let summary = wallet.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_receipt"));
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

/// Bob's shop and second wallet take alice's proof of January; the seller wallet writes January's period
/// record again at a data epoch the shop holds no key of yet; a proof of February, another
/// payment, then reaches the shop. Returns the shop and February's record.
async fn with_a_sealed_record(w: &World) -> (Paykit, [u8; 32]) {
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(w, &mut shop, &mut wallet).await;
    seller_wallet.receive(None).await.unwrap();
    let body = proof_to(w, shop.app_id(), month(0), "lnbc1invoice", 6).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id(), seller_wallet.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );
    shop.receive(None).await.unwrap();
    seller_wallet.receive(None).await.unwrap();
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bob_bundle(
        w,
        seller_wallet.app_id(),
        &seller_wallet.app().cert().clone(),
    );
    seller_wallet.receive(None).await.unwrap();
    rewrites(&mut seller_wallet, &payee_period_at(&request, month(0))).await;
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
    let kept = wallet.app().protocol_scan("paykit", b"pr/").await.unwrap();
    assert_eq!(kept.len(), 2);
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[
                pubky_switch_client::app::LocalWrite::Put {
                    key: kept[0].0.clone(),
                    value: kept[1].1.clone(),
                },
                pubky_switch_client::app::LocalWrite::Put {
                    key: kept[1].0.clone(),
                    value: kept[0].1.clone(),
                },
            ],
        )
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
    let key = format!(
        "kc/{}/{}/{:020}",
        pubky_switch_core::enc::b64(&record_key),
        pubky_switch_core::enc::b64(&shop.app().cert().hash()),
        wallet.app().own_identity().generation(),
    );
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: key.into_bytes(),
                value: b"not a kept cancellation".to_vec(),
            }],
        )
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
async fn test_periods_credit_a_settlement_once_across_payers() {
    check_prior_settlement(true).await;
    check_prior_settlement(false).await;
}

async fn check_prior_settlement(same_settlement: bool) {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let _carol = w
        .join(&w.carol, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let body = period_proof(&w, shop.app_id(), january()).await;
    let mut unrequested = if same_settlement {
        body.clone()
    } else {
        period_proof_for_attempt(
            &w,
            shop.app_id(),
            january(),
            AttemptId::from_bytes([18; 16]),
        )
        .await
    };
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
    let first_receipt = shop.confirm_payment(&carols).await.unwrap().unwrap();
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
    let result = shop.confirm_payment(&sale.periods[0].record).await.unwrap();
    assert_eq!(result.is_some(), !same_settlement);
    assert_ne!(result.as_ref(), Some(&first_receipt));
    assert_eq!(
        shop.confirm_payment(&carols).await.unwrap(),
        Some(first_receipt)
    );
    assert_eq!(
        w.rail.all_executions().values().sum::<u64>(),
        if same_settlement { 1 } else { 2 }
    );
}

#[tokio::test]
async fn test_periods_send_details_never_writes_over_a_period_record_that_doesnt_read() {
    // The seller wallet writes the shop's period record as a value no reader reads, which may hold
    // what a later version stored (Paykit spec 8.1). The shop, whose copy refuses that
    // write, is refused its next details for now, and the value stays as it is.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1a", now + DAY_MS))
        .await
        .unwrap();
    seller_wallet.receive(None).await.unwrap();
    let target = payee_period(&request);
    let held = seller_wallet
        .app_mut()
        .read_record(&target)
        .await
        .unwrap()
        .unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().unwrap()).unwrap();
    value["v"] = json!(99);
    let bytes = serde_json::to_vec(&value).unwrap();
    let op = seller_wallet
        .app_mut()
        .record_op(&target, Some(&bytes), Vec::new())
        .await
        .unwrap();
    seller_wallet
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
    // What the switch holds, as the seller wallet reads it: its own write, not written over.
    let after = seller_wallet
        .app_mut()
        .read_record(&target)
        .await
        .unwrap()
        .unwrap();
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
    policy.write_scopes = vec!["paykit_pay".into(), "paykit_request".into()];
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
    crate::common::restore_with_unknown_cursor(&mut app).await;
    app.resume_payments().await.unwrap();
    app.receive(None).await.unwrap();
    let options = paykit_sdk::AcceptOptions::default()
        .with_possibly_cancelled_confirmed(crate::common::held(&app, &request).await);
    let refused = app
        .accept_subscription_with(&request, options)
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Subscribed);
    let summary = shop.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_request_acceptance"));
}

/// Alice's wallet has subscribed to Bob's monthly request. His wallet sends January's details
/// twice, `apart_ms` apart. Its period record keeps the first in `earlier_details` and the
/// second in `period_details`. Returns the request, Bob's record of it, and
/// the two `msg_id`s, the earlier first.
async fn details_twice(
    w: &World,
    seller_wallet: &mut Paykit,
    wallet: &mut Paykit,
    apart_ms: u64,
) -> (RequestRef, [u8; 32], [String; 2]) {
    let request = requested(w, seller_wallet, wallet).await;
    wallet.accept_subscription(&request).await.unwrap();
    seller_wallet.receive(None).await.unwrap();
    let sale = seller_wallet.payee_activity().await.unwrap()[0].record;
    let first = seller_wallet
        .send_period_details(
            &sale,
            details(january(), "lnbc1first", w.clock.now_ms() + DAY_MS),
        )
        .await
        .unwrap();
    w.clock.advance_ms(apart_ms);
    let second = seller_wallet
        .send_period_details(
            &sale,
            details(january(), "lnbc1second", w.clock.now_ms() + DAY_MS),
        )
        .await
        .unwrap();
    let period = &seller_wallet.payee_activity().await.unwrap()[0].periods[0];
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
async fn test_periods_restore_reports_each_own_details_once() {
    // Both versions have an uncertain delivery after a fresh import.
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
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    for msg_id in &sent {
        assert!(
            crate::common::possibly_delivered(&shop, msg_id).await,
            "{msg_id}"
        );
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
    // Once: a later receive queues nothing more.
    shop.receive(None).await.unwrap();
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
    // The wallet takes the copies as the details it holds.
    wallet.receive(None).await.unwrap();
    let view = &wallet.requests().await.unwrap()[0];
    assert_eq!(view.periods.len(), 1);
    assert_eq!(view.periods[0].details.len(), 2);
}

#[tokio::test]
async fn test_periods_restore_reports_the_original_acceptance() {
    // The shop can retain the original acceptance while its sender reconciles delivery.
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
    assert!(wallet
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    assert!(crate::common::possibly_delivered(&wallet, &accepted.msg_id).await);
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
async fn test_periods_restore_releasing_a_block_keeps_earlier_details_uncertain() {
    // Releasing the block leaves both signed messages with the host to reconcile.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    let (_, _, sent) = details_twice(&w, &mut seller_wallet, &mut wallet, 1_000).await;
    let backup = seller_wallet.backup().await.unwrap();
    drop(seller_wallet);
    let mut seller_wallet = bob_restored(&w, &backup).await;
    seller_wallet.block(&w.alice_id()).await.unwrap();
    let before = w.alice.switch().entries_from(&w.bob_id());
    for _ in 0..2 {
        seller_wallet.receive(None).await.unwrap();
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
    assert!(seller_wallet
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    for msg_id in &sent {
        assert!(!queued(&seller_wallet, msg_id).await, "{msg_id}");
    }
    seller_wallet.unblock(&w.alice_id()).await.unwrap();
    seller_wallet.receive(None).await.unwrap();
    assert!(seller_wallet
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    for msg_id in &sent {
        assert!(
            crate::common::possibly_delivered(&seller_wallet, msg_id).await,
            "{msg_id}"
        );
    }
    // The restore's block mark of alice asks too, once, when its read shows her unblocked.
    let asks = crate::common::asked(&seller_wallet, &w.alice_id())
        .await
        .len();
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + asks);
    seller_wallet.receive(None).await.unwrap();
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before + asks);
}

#[tokio::test]
async fn test_periods_restore_reports_earlier_details_without_a_replay() {
    // The replay deadline does not resolve whether a past delivery reached its recipient.
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
    assert!(crate::common::possibly_delivered(&shop, &sent[0]).await);
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    assert!(!queued(&shop, &sent[0]).await);
    assert!(crate::common::possibly_delivered(&shop, &sent[1]).await);
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
    shop.receive(None).await.unwrap();
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_periods_restore_cancellation_closes_earlier_details() {
    // The cancellation closes both versions of the period details.
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
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    assert!(crate::common::possibly_delivered(&shop, &cancellation).await);
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
}

#[tokio::test]
async fn test_periods_restore_after_the_period_is_credited_queues_no_details() {
    // January is credited after its details were sent, its receipt in its period record: a
    // later stop that covers the details. The restored shop retains the receipt and queues
    // neither it nor the details (Paykit spec 7.2, "What a restore loses").
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
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    assert!(!queued(&shop, &receipt).await);
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
}

#[tokio::test]
async fn test_periods_restore_checks_the_request_before_reporting_its_details() {
    // The parent request must be readable to tell whether its details have been closed.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _, sent) = details_twice(&w, &mut shop, &mut wallet, 1_000).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    seller_wallet.receive(None).await.unwrap();
    let target = payee_request_at(&request);
    let good = seller_wallet
        .app_mut()
        .read_record(&target)
        .await
        .unwrap()
        .unwrap();
    let good = good.content.value().unwrap().to_vec();
    let mut value: serde_json::Value = serde_json::from_slice(&good).unwrap();
    value["v"] = json!(99);
    writes_value(
        &mut seller_wallet,
        &target,
        &serde_json::to_vec(&value).unwrap(),
    )
    .await;
    let mut shop = bob_restored(&w, &backup).await;
    let before = w.alice.switch().entries_from(&w.bob_id());
    for _ in 0..2 {
        shop.receive(None).await.unwrap();
    }
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_some());
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
    writes_value(&mut seller_wallet, &target, &good).await;
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    for msg_id in &sent {
        assert!(
            crate::common::possibly_delivered(&shop, msg_id).await,
            "{msg_id}"
        );
    }
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
    shop.receive(None).await.unwrap();
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
}

#[tokio::test]
async fn test_periods_restore_own_details_waiting_on_their_request_record_end_past_their_window() {
    // The request record stays a value no reader reads: the details wait until both are
    // past the 30 days the client would queue them again in, and then the re-send is owed
    // no more, with nothing sent and nothing told (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _, sent) = details_twice(&w, &mut shop, &mut wallet, 1_000).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    seller_wallet.receive(None).await.unwrap();
    let target = payee_request_at(&request);
    writes_value(
        &mut seller_wallet,
        &target,
        &serde_json::to_vec(&json!({ "v": 99 })).unwrap(),
    )
    .await;
    let mut shop = bob_restored(&w, &backup).await;
    let before = w.alice.switch().entries_from(&w.bob_id());
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_some());
    w.clock.advance_ms(31 * DAY_MS);
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
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
/// request record rewritten by the seller wallet with `edit`. Returns the restored shop, received
/// once, and the details' `msg_id`s.
async fn restored_over_an_answer(
    w: &World,
    edit: impl Fn(&mut serde_json::Value),
) -> (Paykit, [String; 2]) {
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _, sent) = details_twice(w, &mut shop, &mut wallet, 1_000).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    seller_wallet.receive(None).await.unwrap();
    let target = payee_request_at(&request);
    let held = seller_wallet
        .app_mut()
        .read_record(&target)
        .await
        .unwrap()
        .unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().unwrap()).unwrap();
    edit(&mut value);
    writes_value(
        &mut seller_wallet,
        &target,
        &serde_json::to_vec(&value).unwrap(),
    )
    .await;
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
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
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
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
}

#[tokio::test]
async fn test_periods_restore_reports_an_unrelated_request_while_details_wait() {
    // Revisiting waiting details preserves a single notice for an independent request.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _, sent) = details_twice(&w, &mut shop, &mut wallet, 1_000).await;
    let mut unexpiring = draft("1", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let one_time = shop.send_request(w.alice_id(), unexpiring).await.unwrap();
    wallet.receive(None).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    seller_wallet.receive(None).await.unwrap();
    writes_value(
        &mut seller_wallet,
        &payee_request_at(&request),
        &serde_json::to_vec(&json!({ "v": 99 })).unwrap(),
    )
    .await;
    let mut shop = bob_restored(&w, &backup).await;
    let before = w.alice.switch().entries_from(&w.bob_id());
    for _ in 0..3 {
        shop.receive(None).await.unwrap();
    }
    assert!(crate::common::possibly_delivered(&shop, &one_time.msg_id).await);
    for msg_id in &sent {
        assert!(!queued(&shop, msg_id).await, "{msg_id}");
    }
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_some());
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
}

#[tokio::test]
async fn test_periods_credit_stays_given_after_a_restore() {
    // The credited period retains its receipt and no new delivery is created.
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
    // The issued receipt stays available, with no delivery created by the restore.
    assert!(!queued(&shop, &receipt).await);
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
pub(super) fn payer_period_at(request: &RequestRef, period: PeriodRef) -> RecordTarget {
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
    // Bob's wallet writes, at the shop's January period record, February's record: a value
    // that reads but doesn't bind to that key (Paykit wire 5.3). Alice's proof of January
    // then reaches the shop. It is never written over that value: it is kept as a reply
    // before its record, and given up with the host's notice past its 30 days.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(month(1), "lnbc1feb", now + DAY_MS))
        .await
        .unwrap();
    seller_wallet.receive(None).await.unwrap();
    let february = seller_wallet
        .app_mut()
        .read_record(&payee_period_at(&request, month(1)))
        .await
        .unwrap()
        .unwrap();
    let january_record = payee_period(&request);
    let op = seller_wallet
        .app_mut()
        .record_op(
            &january_record,
            Some(february.content.value().unwrap()),
            Vec::new(),
        )
        .await
        .unwrap();
    seller_wallet
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let key = shop.app().record_key(&january_record).unwrap();
    let seller_wallets = shop.app().shared_record(&key).await.unwrap().unwrap();
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
    assert_eq!(after.version, seller_wallets.version);
    assert_eq!(after.content, seller_wallets.content);
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
    // refused for good twice, so it stays a stored entry. The seller wallet then deletes the period
    // record (Paykit spec 6 allows deleting an activity record). The later write never
    // opens a new period record holding the proof without the details it names: the entry
    // waits, and past its window it is given up, the host told once.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
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
    seller_wallet.receive(None).await.unwrap();
    let op = seller_wallet
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    seller_wallet
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
    // Bob's wallet cancels the subscription; the shop hasn't read that write. Its details are
    // judged on the request record read again, which shows the cancellation: refused, and
    // nothing reaches the wallet.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    seller_wallet.receive(None).await.unwrap();
    let seller_wallets = seller_wallet.payee_activity().await.unwrap()[0].record;
    seller_wallet
        .cancel_sent_request(&seller_wallets, None)
        .await
        .unwrap();
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
    // The shop holds January's proof in its period record. Bob's wallet writes the shop's
    // request record again at a data epoch the shop holds no key of yet: the credit is
    // refused for now, not as a record not found, and given once the key comes.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
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
    seller_wallet.receive(None).await.unwrap();
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bob_bundle(
        &w,
        seller_wallet.app_id(),
        &seller_wallet.app().cert().clone(),
    );
    seller_wallet.receive(None).await.unwrap();
    let collection = Collection::PayeeActivity;
    let request_record = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    );
    rewrites(&mut seller_wallet, &request_record).await;
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
    // Bob's wallet holds `paykit_contacts`. His shop writes, at the seller wallet's January period record,
    // February's record: a value that reads but doesn't bind to that key (Paykit wire 5.3).
    // Alice is flagged since, and her proof of a real payment of January reaches the seller wallet.
    // It updates a subscription the seller wallet holds, no new state, so the spam rule doesn't gate
    // it: it is kept, never written over that value, and given up with the host's notice
    // past its 30 days.
    let w = World::new();
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut seller_wallet = w
        .join_with(
            &w.bob,
            "seller-wallet",
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
    seller_wallet.receive(None).await.unwrap();
    let request = seller_wallet
        .send_request(w.alice_id(), monthly(w.clock.now_ms(), None))
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    seller_wallet.receive(None).await.unwrap();
    let sale = seller_wallet.payee_activity().await.unwrap()[0].record;
    let now = w.clock.now_ms();
    seller_wallet
        .send_period_details(&sale, details(month(1), "lnbc1feb", now + DAY_MS))
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    seller_wallet.receive(None).await.unwrap();
    let key = seller_wallet.app().record_key(&january_record).unwrap();
    let shops = seller_wallet
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap();
    lists.flag(&w.alice_id());
    let body = period_proof(&w, seller_wallet.app_id(), january()).await;
    let msg_id = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[seller_wallet.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );
    let summary = seller_wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_proof"));
    seller_wallet.receive(None).await.unwrap();
    assert_eq!(seller_wallet.spam_count(&w.alice_id()).await.unwrap(), 0);
    let after = seller_wallet
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.version, shops.version);
    assert_eq!(after.content, shops.content);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    seller_wallet.receive(None).await.unwrap();
    let notices = seller_wallet.notices().await.unwrap();
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
    // Carol, flagged, isn't the counterparty of alice's subscription at Bob's wallet, yet she
    // sends a proof naming its request and period details the seller wallet doesn't hold. The spam
    // rule gates a message from a sender that is no counterparty of the record it names: it
    // is only counted, and nothing of hers is kept.
    let w = World::new();
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut seller_wallet = w
        .join_with(
            &w.bob,
            "seller-wallet",
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
    seller_wallet.receive(None).await.unwrap();
    let request = seller_wallet
        .send_request(w.alice_id(), monthly(w.clock.now_ms(), None))
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&request).await.unwrap();
    seller_wallet.receive(None).await.unwrap();
    lists.flag(&w.carol_id());
    let body = period_proof(&w, seller_wallet.app_id(), january()).await;
    w.inject(
        &w.carol,
        "wallet",
        &w.bob,
        &[seller_wallet.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash, [1; 32]],
        body,
    );
    seller_wallet.receive(None).await.unwrap();
    seller_wallet.receive(None).await.unwrap();
    let kept = seller_wallet
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap();
    assert!(kept.is_empty(), "{} kept", kept.len());
    assert_eq!(seller_wallet.spam_count(&w.carol_id()).await.unwrap(), 1);
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
    // Bob's wallet writes the shop's request record as a value sealed under a key that isn't
    // the epoch's: the shop holds the key, so it never opens. Alice's proof of January then
    // reaches the shop: whether she is the record's counterparty can't be told, so it is kept
    // as a reply before its record at once, under the spam rule, never written over the
    // value (Paykit spec 8.1, "Replies before their record"). Past 30 days from its
    // acceptance it is given up with the host's notice.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    seller_wallet.receive(None).await.unwrap();
    // A new data epoch whose key only the seller wallet holds yet.
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bob_bundle(
        &w,
        seller_wallet.app_id(),
        &seller_wallet.app().cert().clone(),
    );
    seller_wallet.receive(None).await.unwrap();
    let collection = Collection::PayeeActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    );
    let value = seller_wallet
        .app_mut()
        .read_record(&target)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    let (conditions, kind) = seller_wallet
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
    seller_wallet
        .app_mut()
        .submit_batch(
            OpBatch::for_protocol("paykit")
                .entry((conditions, pubky_switch_core::ops::OpKind::RecordPut(put))),
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
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    for _ in 0..3 {
        shop.receive(None).await.unwrap();
    }
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
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

/// The wallet asks for January's dropped details before their request record reads.
/// When the payee is blocked before that ask, it waits until the unblock. Once the
/// request record reads, the wallet can place the re-sent details.
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
                .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
    if blocked {
        wallet.app_mut().receive(None).await.unwrap();
        wallet.block(&w.bob_id()).await.unwrap();
    }
    let dropped = more + 1 > paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    assert_eq!(
        wallet.receive(None).await.unwrap().resends,
        u64::from(dropped && !blocked)
    );
    let marks: Vec<serde_json::Value> = wallet
        .app()
        .protocol_scan("paykit", b"pd/")
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
        assert_eq!(marks[0]["due"], json!(true), "{record_while:?}");
        assert_eq!(marks[0]["earliest_ms"], json!(first_at), "{record_while:?}");
    }
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    match record_while {
        RecordWhile::Gone => {
            let op = phone
                .app_mut()
                .record_op(&target, Some(&good), Vec::new())
                .await
                .unwrap();
            phone
                .app_mut()
                .submit_batch(OpBatch::for_protocol("paykit").entry(op))
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
    assert_eq!(
        summary.resends,
        u64::from(dropped && blocked),
        "{record_while:?}"
    );
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
async fn test_periods_details_dropped_for_the_bounds_asked_with_only_a_period_record() {
    // February's record reads while the request record for the dropped details does not.
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
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"dw/")
        .await
        .unwrap()
        .is_empty());
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
    assert!(shop
        .app()
        .protocol_scan("paykit", b"dw/")
        .await
        .unwrap()
        .is_empty());
    crate::divergent::confirm(&mut shop, &period).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap()[0].periods[0].paid);
}

#[tokio::test]
async fn test_periods_invalid_details_never_remove_the_request_destination_constraint() {
    // Fixed request endpoints and no conversion terms: discarding invalid named details
    // must not erase the destination constraint. A payment to an unrelated destination
    // is refused; one to the request's endpoint still counts where that endpoint applies.
    for per_period in [false, true] {
        for period_index in [0, 1] {
            for permitted in [false, true] {
                let w = World::new();
                let mut shop = w.shop().await;
                let mut wallet = w.wallet("wallet").await;
                let now = w.clock.now_ms();
                let mut draft = monthly(now, None);
                draft.per_period_endpoints = per_period;
                let (request, sale) = subscribed_for(
                    &w,
                    &mut shop,
                    &mut wallet,
                    draft,
                    if period_index == 0 { 0 } else { 40 },
                )
                .await;
                shop.send_period_details(
                    &sale,
                    details(
                        month(period_index),
                        "lnbc1february",
                        w.clock.now_ms() + DAY_MS,
                    ),
                )
                .await
                .unwrap();
                let target = payee_period_at(&request, month(period_index));
                let held = shop.app_mut().read_record(&target).await.unwrap().unwrap();
                let mut record = paykit_lib::records::ActivityRecord::read(
                    held.content.value().unwrap(),
                    &w.bob_id(),
                    paykit_lib::records::ActivitySide::Payee,
                )
                .unwrap();
                w.clock.advance_ms(1);
                let mut body =
                    details(month(period_index), "lnbc1rated", w.clock.now_ms() + DAY_MS)
                        .to_body()
                        .unwrap();
                body["payment_endpoints"] = json!({
                    "btc-onchain-address": "btc1named",
                    "usd-ach-account": "usd1named"
                });
                body.insert(
                    "rates".into(),
                    json!([{ "asset": "usd", "value": "100000" }]),
                );
                let plaintext = w.sign_as(
                    &w.bob,
                    "shop",
                    &w.alice,
                    "paykit.payment_period_details",
                    vec![request.request_hash],
                    body,
                );
                let rated = paykit_lib::records::StoredMessage::new(
                    w.bob_id(),
                    shop.app().cert().signed(),
                    plaintext,
                    None,
                    &w.alice_id(),
                )
                .unwrap();
                let valid = record.period_details.replace(rated.clone()).unwrap();
                record.earlier_details.push(valid);
                record.msg_ids.push(rated.message().msg_id.clone());
                let op = shop
                    .app_mut()
                    .record_op(&target, Some(&record.to_json().unwrap()), Vec::new())
                    .await
                    .unwrap();
                shop.app_mut()
                    .submit_batch(OpBatch::for_protocol("paykit").entry(op))
                    .await
                    .unwrap();
                let (identifier, destination) = if permitted {
                    ("btc-lightning-bolt11", "lnbc1invoice")
                } else {
                    ("btc-onchain-address", "btc1unrelated")
                };
                let attempt = AttemptId::from_bytes([9; 16]);
                let RailOutcome::Paid(paid) = w
                    .rail
                    .pay_outside(
                        endpoint(identifier),
                        PaymentEndpointPayload::new(destination).unwrap(),
                        btc("1"),
                        attempt,
                    )
                    .await
                    .unwrap()
                else {
                    panic!("payment must complete")
                };
                let body = PaymentProof {
                    attempt_id: attempt,
                    billing_period: Some(month(period_index)),
                    payment_endpoint_identifier: endpoint(identifier),
                    payment_app_id: shop.app_id(),
                    list_version: None,
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
                    vec![request.request_hash, rated.hash()],
                    body,
                );
                shop.receive(None).await.unwrap();
                let february = |periods: Vec<paykit_sdk::PeriodView>| {
                    periods
                        .into_iter()
                        .find(|p| p.billing_period == month(period_index))
                        .unwrap()
                };
                let period = february(shop.payee_activity().await.unwrap()[0].periods.clone());
                // Only the details valid against the request show.
                assert_eq!(period.details.len(), 1);
                assert!(period.paid);
                let verified = shop.verify_payment(&period.record).await.unwrap();
                let confirmed = shop.confirm_payment(&period.record).await.unwrap();
                let period = february(shop.payee_activity().await.unwrap()[0].periods.clone());
                let summary = wallet.receive(None).await.unwrap();
                let sent = received(&summary, "paykit.payment_receipt");
                let expected = permitted && (!per_period || period_index == 0);
                assert_eq!(
                    (
                        verified.is_some(),
                        confirmed.is_some(),
                        period.credited,
                        sent
                    ),
                    (expected, expected, expected, expected),
                    "per_period={per_period}, period={period_index}, permitted={permitted}",
                );
            }
        }
    }
}

#[tokio::test]
async fn test_periods_credit_requires_the_selected_endpoint() {
    // A selected details map replaces the request map as a whole. An identifier missing
    // from it never falls back to the request; unused details do not restrict the request.
    for per_period in [false, true] {
        for period_index in [0, 1] {
            for named in [false, true] {
                for permitted in [false, true] {
                    let w = World::new();
                    let mut shop = w.shop().await;
                    let mut wallet = w.wallet("wallet").await;
                    let mut draft = monthly(w.clock.now_ms(), None);
                    draft.per_period_endpoints = per_period;
                    let endpoints = BTreeMap::from([(
                        endpoint(if permitted {
                            "btc-lightning-bolt11"
                        } else {
                            "btc-onchain-address"
                        }),
                        PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
                    )]);
                    if !named {
                        draft.destination = RequestDestination::Endpoints(endpoints.clone());
                    }
                    let (request, sale) = subscribed_for(
                        &w,
                        &mut shop,
                        &mut wallet,
                        draft,
                        if period_index == 0 { 0 } else { 40 },
                    )
                    .await;
                    let period = month(period_index);
                    let mut refs = vec![request.request_hash];
                    if named {
                        let mut terms = details(period, "lnbc1invoice", w.clock.now_ms() + DAY_MS);
                        terms.payment_endpoints = Some(endpoints);
                        shop.send_period_details(&sale, terms).await.unwrap();
                        let held = shop
                            .app_mut()
                            .read_record(&payee_period_at(&request, period))
                            .await
                            .unwrap()
                            .unwrap();
                        let record = paykit_lib::records::ActivityRecord::read(
                            held.content.value().unwrap(),
                            &w.bob_id(),
                            paykit_lib::records::ActivitySide::Payee,
                        )
                        .unwrap();
                        refs.push(record.period_details.unwrap().hash());
                    }
                    let body = proof_to(&w, shop.app_id(), period, "lnbc1invoice", 10).await;
                    w.inject(
                        &w.alice,
                        "wallet",
                        &w.bob,
                        &[shop.app_id()],
                        "paykit.payment_proof",
                        refs,
                        body,
                    );
                    shop.receive(None).await.unwrap();
                    let record = shop.payee_activity().await.unwrap()[0].periods[0].record;
                    let verified = shop.verify_payment(&record).await.unwrap().is_some();
                    let confirmed = shop.confirm_payment(&record).await.unwrap().is_some();
                    let credited = shop.payee_activity().await.unwrap()[0].periods[0].credited;
                    let receipt = received(
                        &wallet.receive(None).await.unwrap(),
                        "paykit.payment_receipt",
                    );
                    let expected = if per_period && period_index != 0 {
                        named && permitted
                    } else {
                        named || permitted
                    };
                    assert_eq!((verified, confirmed, credited, receipt),
                        (expected, expected, expected, expected),
                        "per_period={per_period}, period={period_index}, named={named}, permitted={permitted}");
                }
            }
        }
    }
}

#[tokio::test]
async fn test_periods_credit_without_fixed_endpoints_respects_request_restrictions() {
    // No fixed payload lets the payment method check its own destination. The request's
    // identifier restriction and required app still apply.
    for required_app in [None, Some(true), Some(false)] {
        for permitted in [false, true] {
            let w = World::new();
            let mut shop = w.shop().await;
            let mut wallet = w.wallet("wallet").await;
            let mut draft = monthly(w.clock.now_ms(), None);
            draft.destination = match required_app {
                None => RequestDestination::Any,
                Some(true) => RequestDestination::RequiredApp(shop.app_id()),
                Some(false) => RequestDestination::RequiredApp([0x3f; 16]),
            };
            draft.accepted_endpoint_identifiers = Some(vec![endpoint(if permitted {
                "btc-lightning-bolt11"
            } else {
                "btc-onchain-address"
            })]);
            let (request, _) = subscribed_for(&w, &mut shop, &mut wallet, draft, 0).await;
            let body = proof_to(&w, shop.app_id(), january(), "lnbc1invoice", 11).await;
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
            let verified = shop.verify_payment(&record).await.unwrap().is_some();
            let confirmed = shop.confirm_payment(&record).await.unwrap().is_some();
            let credited = shop.payee_activity().await.unwrap()[0].periods[0].credited;
            let receipt = received(
                &wallet.receive(None).await.unwrap(),
                "paykit.payment_receipt",
            );
            let expected = permitted && required_app != Some(false);
            assert_eq!(
                (verified, confirmed, credited, receipt),
                (expected, expected, expected, expected),
                "required_app={required_app:?}, permitted={permitted}"
            );
        }
    }
}

#[tokio::test]
async fn test_periods_credit_requires_the_named_endpoint_owner() {
    check_period_endpoint_owner(true).await;
}

#[tokio::test]
async fn test_periods_rates_only_details_keep_the_request_endpoint_owner() {
    check_period_endpoint_owner(false).await;
}

async fn check_period_endpoint_owner(details_endpoints: bool) {
    use paykit_lib::{Asset, Decimal, Rate};

    // Details are signed by another current app. Only a later period with per-period
    // endpoints takes that destination and owner; otherwise the request supplies both.
    for period_index in [0, 1] {
        for correct_owner in [false, true] {
            let w = World::new();
            let mut shop = w.shop().await;
            let mut seller_wallet = w
                .join(
                    &w.bob,
                    "seller-wallet",
                    AppScopes::new().request().build().unwrap(),
                )
                .await;
            let mut wallet = w.wallet("wallet").await;
            let (request, sale) =
                subscribed_with_conversion(&w, &mut shop, &mut wallet, details_endpoints).await;
            if period_index != 0 {
                w.clock.advance_ms(40 * DAY_MS);
            }
            seller_wallet.receive(None).await.unwrap();
            let period = month(period_index);
            let mut terms = details(period, "lnbc1details", w.clock.now_ms() + DAY_MS);
            if !details_endpoints {
                terms.payment_endpoints = None;
                terms.rates = Some(vec![Rate::new(
                    Asset::new("usd").unwrap(),
                    Decimal::new("1").unwrap(),
                )
                .unwrap()]);
            }
            seller_wallet
                .send_period_details(&sale, terms)
                .await
                .unwrap();
            let held = seller_wallet
                .app_mut()
                .read_record(&payee_period_at(&request, period))
                .await
                .unwrap()
                .unwrap();
            let record = paykit_lib::records::ActivityRecord::read(
                held.content.value().unwrap(),
                &w.bob_id(),
                paykit_lib::records::ActivitySide::Payee,
            )
            .unwrap();
            let named = record.period_details.unwrap();
            assert_eq!(named.message().from.app_id, seller_wallet.app_id());
            let uses_details = details_endpoints && period_index != 0;
            let (owner, other) = if uses_details {
                (seller_wallet.app_id(), shop.app_id())
            } else {
                (shop.app_id(), seller_wallet.app_id())
            };
            let payment_app = if correct_owner { owner } else { other };
            let invoice = if uses_details {
                "lnbc1details"
            } else {
                "lnbc1invoice"
            };
            let body = proof_to(&w, payment_app, period, invoice, 14).await;
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
            let period_record = shop.payee_activity().await.unwrap()[0].periods[0].record;
            let verified = shop.verify_payment(&period_record).await.unwrap().is_some();
            let confirmed = shop
                .confirm_payment(&period_record)
                .await
                .unwrap()
                .is_some();
            let credited = shop.payee_activity().await.unwrap()[0].periods[0].credited;
            let receipt = received(
                &wallet.receive(None).await.unwrap(),
                "paykit.payment_receipt",
            );
            assert_eq!((verified, confirmed, credited, receipt), (correct_owner, correct_owner, correct_owner, correct_owner), "details_endpoints={details_endpoints}, period={period_index}, correct_owner={correct_owner}");
            assert_eq!(
                w.rail.proof_checks().len(),
                if correct_owner { 2 } else { 0 }
            );
        }
    }
}

/// A request with per-period conversion terms, signed and recorded by the payee before
/// delivery, as a sending app outside this SDK may provide it (Paykit wire 4.5).
async fn subscribed_with_conversion(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    per_period_endpoints: bool,
) -> (RequestRef, [u8; 32]) {
    use paykit_lib::bodies::{Conversion, PaymentRequest};
    use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};

    let now = w.clock.now_ms();
    let draft = monthly(now, None);
    let body = PaymentRequest {
        amount: draft.amount,
        payment_reference: draft.payment_reference,
        proposal_expires_at: draft.proposal_expires_at_ms,
        payment_deadline: None,
        recurrence: draft.recurrence,
        destination: draft.destination,
        per_period_endpoints,
        label: None,
        accepted_payment_endpoint_identifiers: Some(vec![
            endpoint("btc-lightning-bolt11"),
            endpoint("usd-ach-account"),
        ]),
        conversion: Some(Conversion::PerPeriod),
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
    let stored = StoredMessage::new(
        w.bob_id(),
        shop.app().cert().signed(),
        plaintext.clone(),
        None,
        &w.alice_id(),
    )
    .unwrap();
    let request = RequestRef {
        payee: w.bob_id(),
        request_hash: stored.hash(),
    };
    let mut record = ActivityRecord::new(ActivitySide::Payee, w.alice_id());
    record.msg_ids.push(stored.message().msg_id.clone());
    record.request = Some(stored);
    let op = shop
        .app_mut()
        .record_op(
            &payee_request_at(&request),
            Some(&record.to_json().unwrap()),
            Vec::new(),
        )
        .await
        .unwrap();
    shop.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let accepted = accepted_by(w, wallet, &plaintext, request.request_hash).await;
    assert_eq!(accepted, request);
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    (request, sale)
}

#[tokio::test]
async fn test_periods_named_endpoints_keep_the_required_app_constraint() {
    for per_period in [false, true] {
        for period_index in [0, 1] {
            for permitted_owner in [false, true] {
                let w = World::new();
                let mut shop = w.shop().await;
                let mut seller_wallet = w
                    .join(
                        &w.bob,
                        "seller-wallet",
                        AppScopes::new().request().build().unwrap(),
                    )
                    .await;
                let mut wallet = w.wallet("wallet").await;
                let mut draft = monthly(w.clock.now_ms(), None);
                draft.per_period_endpoints = per_period;
                draft.destination = RequestDestination::RequiredApp(if permitted_owner {
                    seller_wallet.app_id()
                } else {
                    shop.app_id()
                });
                let (request, sale) = subscribed_for(
                    &w,
                    &mut shop,
                    &mut wallet,
                    draft,
                    if period_index == 0 { 0 } else { 40 },
                )
                .await;
                seller_wallet.receive(None).await.unwrap();
                let period = month(period_index);
                seller_wallet
                    .send_period_details(
                        &sale,
                        details(period, "lnbc1details", w.clock.now_ms() + DAY_MS),
                    )
                    .await
                    .unwrap();
                let held = seller_wallet
                    .app_mut()
                    .read_record(&payee_period_at(&request, period))
                    .await
                    .unwrap()
                    .unwrap();
                let record = paykit_lib::records::ActivityRecord::read(
                    held.content.value().unwrap(),
                    &w.bob_id(),
                    paykit_lib::records::ActivitySide::Payee,
                )
                .unwrap();
                let named = record.period_details.unwrap();
                assert_eq!(named.message().from.app_id, seller_wallet.app_id());
                let body = proof_to(&w, seller_wallet.app_id(), period, "lnbc1details", 15).await;
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
                let sale = shop.payee_activity().await.unwrap().remove(0);
                assert_eq!(sale.periods[0].details.len(), 1);
                let record = sale.periods[0].record;
                let verified = shop.verify_payment(&record).await.unwrap().is_some();
                let confirmed = shop.confirm_payment(&record).await.unwrap().is_some();
                let credited = shop.payee_activity().await.unwrap()[0].periods[0].credited;
                let receipt = received(
                    &wallet.receive(None).await.unwrap(),
                    "paykit.payment_receipt",
                );
                assert_eq!((verified, confirmed, credited, receipt), (permitted_owner, permitted_owner, permitted_owner, permitted_owner), "per_period={per_period}, period={period_index}, permitted_owner={permitted_owner}");
                assert_eq!(
                    w.rail.proof_checks().len(),
                    if permitted_owner { 2 } else { 0 }
                );
            }
        }
    }
}

#[tokio::test]
async fn test_payee_deletion_rejects_a_signed_receipt_for_a_different_period_start() {
    let mut period = january();
    period.starts_at += 1;
    // The activity key uses whole seconds. The signed start still belongs to this key.
    assert_eq!(period.start_secs(), january().start_secs());
    Box::pin(credit_after_wrong_period(period, false)).await;
}

#[tokio::test]
async fn test_payee_deletion_rejects_a_signed_receipt_for_a_different_period_end() {
    let mut period = january();
    period.ends_at += 1;
    Box::pin(credit_after_wrong_period(period, false)).await;
}

#[tokio::test]
async fn test_payee_deletion_a_missing_signed_request_leaves_prior_credit_unresolved() {
    let mut period = january();
    period.ends_at += 1;
    Box::pin(credit_after_wrong_period(period, true)).await;
}

/// Alice's payment to the fixed invoice with the proof and receipt signed by both apps.
async fn signed_period_payment(
    w: &World,
    shop: &Paykit,
    wallet: &Paykit,
    request: &RequestRef,
    period: PeriodRef,
) -> paykit_lib::records::ActivityRecord {
    use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
    let proof = period_proof(w, shop.app_id(), period).await;
    let plaintext = w.sign_as(
        &w.alice,
        "wallet",
        &w.bob,
        "paykit.payment_proof",
        vec![request.request_hash],
        proof,
    );
    let proof = StoredMessage::new(
        w.alice_id(),
        wallet.app().cert().signed(),
        plaintext,
        Some(w.clock.now_ms()),
        &w.bob_id(),
    )
    .unwrap();
    let receipt = crate::receipts::signed_receipt(w, shop, request, &proof);
    let mut record = ActivityRecord::new(ActivitySide::Payee, w.alice_id());
    record.proof = Some(proof);
    record.receipt = Some(receipt);
    record
}

async fn credit_after_wrong_period(period: PeriodRef, missing_request: bool) {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let mut record = signed_period_payment(&w, &shop, &wallet, &request, period).await;
    let mut elsewhere = record.proof.as_ref().unwrap().message().body.clone();
    record.lost_to_delete = Some([7; 32]);
    let target = payee_period_at(&request, january());
    let key = shop.app().record_key(&target).unwrap();
    writes_value(&mut shop, &target, &record.to_json().unwrap()).await;
    assert!(matches!(
        shop.confirm_payment_with(
            &key,
            paykit_sdk::ReceiptDetails::new()
                .with_maybe_credited_confirmed(pubky_switch_client::app::UserConfirmed::new())
        )
        .await
        .unwrap_err()
        .refusal(),
        Some(Refusal::CreditHistoryLost)
    ));
    seller_wallet.receive(None).await.unwrap();
    let parent_target = payee_request_at(&request);
    let parent_key = shop.app().record_key(&parent_target).unwrap();
    let parent = seller_wallet
        .app()
        .shared_record(&parent_key)
        .await
        .unwrap()
        .unwrap()
        .content;
    if missing_request {
        seller_wallet
            .app_mut()
            .delete_record(&parent_target, Vec::new())
            .await
            .unwrap();
        shop.app_mut()
            .read_record_fresh(&parent_target)
            .await
            .unwrap();
    }
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    let before = w.record_writes(&w.bob, &key);
    shop.receive(None).await.unwrap();
    assert!(shop.retained_payee_receipts().await.unwrap().is_empty());
    assert_eq!(w.record_writes(&w.bob, &key), before);
    let req = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap()
        .request
        .unwrap();
    elsewhere.remove("billing_period");
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![req.request_hash],
        elsewhere,
    );
    shop.receive(None).await.unwrap();
    let other = shop.app().record_key(&payee_request_at(&req)).unwrap();
    let confirmed = || {
        paykit_sdk::ReceiptDetails::new()
            .with_maybe_credited_confirmed(pubky_switch_client::app::UserConfirmed::new())
    };
    if missing_request {
        let count = w.record_writes(&w.bob, &other);
        assert!(matches!(
            shop.confirm_payment_with(&other, confirmed())
                .await
                .unwrap_err()
                .refusal(),
            Some(Refusal::NotNow(_))
        ));
        assert_eq!(w.record_writes(&w.bob, &other), count);
        seller_wallet
            .app_mut()
            .put_record(&parent_target, parent.value().unwrap(), Vec::new())
            .await
            .unwrap();
        shop.receive(None).await.unwrap();
    }
    assert!(shop
        .confirm_payment_with(&other, confirmed())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_payee_deletion_a_retained_receipt_blocks_the_same_settlement_for_any_payer() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut carol_wallet = w
        .join(&w.carol, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    carol_wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let proof = period_proof(&w, shop.app_id(), january()).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        proof.clone(),
    );
    shop.receive(None).await.unwrap();
    let target = payee_period_at(&request, january());
    let key = shop.app().record_key(&target).unwrap();
    let receipt = shop.confirm_payment(&key).await.unwrap().unwrap();
    seller_wallet.receive(None).await.unwrap();
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.retained_payee_receipts().await.unwrap()[0]
            .receipt
            .message()
            .msg_id,
        receipt
    );

    let mut unrequested = proof;
    unrequested.remove("billing_period");
    unrequested.insert("amount".into(), json!({"asset":"btc","value":"1"}));
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        Vec::new(),
        unrequested.clone(),
    );
    let carols = shop
        .send_request(w.carol_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap()
        .request
        .unwrap();
    unrequested.remove("amount");
    w.inject(
        &w.carol,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![carols.request_hash],
        unrequested,
    );
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    let alice = sales
        .iter()
        .find(|s| s.payer == w.alice_id() && s.request_hash.is_none())
        .unwrap()
        .record;
    let carol = sales
        .iter()
        .find(|s| s.payer == w.carol_id())
        .unwrap()
        .record;
    let before = w.record_writes(&w.bob, &alice);
    let confirmed = paykit_sdk::ReceiptDetails::new()
        .with_maybe_credited_confirmed(pubky_switch_client::app::UserConfirmed::new());
    assert_eq!(
        shop.confirm_payment_with(&alice, confirmed).await.unwrap(),
        None
    );
    assert_eq!(w.record_writes(&w.bob, &alice), before);
    let confirmed = paykit_sdk::ReceiptDetails::new()
        .with_maybe_credited_confirmed(pubky_switch_client::app::UserConfirmed::new());
    let before = w.record_writes(&w.bob, &carol);
    assert_eq!(
        shop.confirm_payment_with(&carol, confirmed).await.unwrap(),
        None
    );
    assert_eq!(w.record_writes(&w.bob, &carol), before);
    let independent = shop
        .send_request(w.carol_id(), draft("0.0005", w.clock.now_ms()))
        .await
        .unwrap()
        .request
        .unwrap();
    carol_wallet.receive(None).await.unwrap();
    carol_wallet
        .pay_request(&independent, paykit_sdk::PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let key = shop
        .app()
        .record_key(&payee_request_at(&independent))
        .unwrap();
    assert!(shop.confirm_payment(&key).await.unwrap().is_some());
}

#[tokio::test]
async fn test_payee_deletion_an_offline_period_receipt_keeps_its_original_signed_request() {
    let w = World::new();
    let device = store();
    let scopes = || AppScopes::new().request().build().unwrap();
    let mut shop = w
        .join_with(&w.bob, "shop", scopes(), Default::default(), device.clone())
        .await;
    let backup = shop.backup().await.unwrap();
    let mut wallet = w.wallet("wallet").await;
    let mut seller_wallet = w.join(&w.bob, "seller-wallet", scopes()).await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let proof = period_proof(&w, shop.app_id(), january()).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        proof,
    );
    shop.receive(None).await.unwrap();
    let target = payee_period_at(&request, january());
    let key = shop.app().record_key(&target).unwrap();
    let original = shop.confirm_payment(&key).await.unwrap().unwrap();
    seller_wallet.receive(None).await.unwrap();
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    seller_wallet
        .app_mut()
        .delete_record(&payee_request_at(&request), Vec::new())
        .await
        .unwrap();
    let before = w.record_writes(&w.bob, &key);
    drop(shop);
    let mut shop = Paykit::restore(
        w.env(&w.bob, device),
        Default::default(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    let kept = shop.retained_payee_receipts().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].receipt.message().msg_id, original);
    assert!(kept[0].deletions.is_empty());
    assert!(shop.payee_deletion_notices().await.unwrap().is_empty());
    assert_eq!(w.record_writes(&w.bob, &key), before);
}

#[tokio::test]
async fn test_payee_deletion_a_period_confirmation_rereads_its_changed_request() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let proof = period_proof(&w, shop.app_id(), january()).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        proof,
    );
    shop.receive(None).await.unwrap();
    seller_wallet.receive(None).await.unwrap();
    let target = payee_period_at(&request, january());
    let period = shop.app().record_key(&target).unwrap();
    let parent = payee_request_at(&request);
    let key = seller_wallet.app().record_key(&parent).unwrap();
    let value = seller_wallet
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content;
    let mut marked = paykit_lib::records::ActivityRecord::read(
        value.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap();
    marked.lost_to_delete = Some([79; 32]);
    seller_wallet
        .app_mut()
        .put_record(&parent, &marked.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    let checks = w.rail.proof_checks().len();
    let writes = w.record_writes(&w.bob, &period);
    assert!(matches!(
        shop.confirm_payment(&period).await.unwrap_err().refusal(),
        Some(Refusal::CreditHistoryLost)
    ));
    assert_eq!(w.rail.proof_checks().len(), checks + 1);
    assert_eq!(w.record_writes(&w.bob, &period), writes);
    let held = shop
        .app()
        .shared_record(&period)
        .await
        .unwrap()
        .unwrap()
        .content;
    assert!(paykit_lib::records::ActivityRecord::read(
        held.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee
    )
    .unwrap()
    .receipt
    .is_none());
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .receipts(paykit_sdk::ReceiptsOf::All)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_periods_shared_payment_must_belong_to_the_signed_request() {
    Box::pin(shared_period_payment()).await;
}

async fn shared_period_payment() {
    use paykit_lib::records::ActivitySide;
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let mut period = january();
    period.ends_at += 1;
    let mut record = signed_period_payment(&w, &shop, &wallet, &request, period).await;
    let receipt = record.receipt.take();
    let target = payee_period_at(&request, january());
    let key = shop.app().record_key(&target).unwrap();
    writes_value(&mut shop, &target, &record.to_json().unwrap()).await;

    let checks = w.rail.proof_checks().len();
    let writes = w.record_writes(&w.bob, &key);
    let entries = w.alice.switch().entries_from(&w.bob_id());
    assert!(shop.payee_activity().await.unwrap()[0].periods.is_empty());
    assert_eq!(shop.verify_payment(&key).await.unwrap(), None);
    assert_eq!(shop.confirm_payment(&key).await.unwrap(), None);
    assert_eq!(w.record_writes(&w.bob, &key), writes);
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), entries);
    assert_eq!(w.rail.proof_checks().len(), checks);

    record.receipt = receipt;
    writes_value(&mut shop, &target, &record.to_json().unwrap()).await;
    assert!(shop.payee_activity().await.unwrap()[0].periods.is_empty());
    assert_eq!(shop.confirm_payment(&key).await.unwrap(), None);
    record.side = ActivitySide::Payer;
    record.counterparty = w.bob_id();
    let target = payer_period_at(&request, january());
    writes_value(&mut wallet, &target, &record.to_json().unwrap()).await;
    assert!(wallet.request(&request).await.unwrap().periods.is_empty());
    assert!(wallet.requests().await.unwrap()[0].periods.is_empty());
    assert!(wallet
        .receipts(paykit_sdk::ReceiptsOf::All)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_periods_receipt_is_attributed_when_its_signed_request_arrives() {
    use paykit_lib::records::ActivitySide;
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    wallet.receive(None).await.unwrap();
    let request = shop
        .send_request(w.alice_id(), monthly(w.clock.now_ms(), None))
        .await
        .unwrap()
        .request
        .unwrap();
    let mut record = signed_period_payment(&w, &shop, &wallet, &request, january()).await;
    record.side = ActivitySide::Payer;
    record.counterparty = w.bob_id();
    let target = payer_period_at(&request, january());
    writes_value(&mut wallet, &target, &record.to_json().unwrap()).await;
    let of = paykit_sdk::ReceiptsOf::Period(request, january());
    assert!(wallet.receipts(of.clone()).await.unwrap().is_empty());
    wallet.receive(None).await.unwrap();
    let receipts = wallet.receipts(of).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].msg_id,
        record.receipt.as_ref().unwrap().message().msg_id
    );
    assert_eq!(receipts[0].payment_reference.as_deref(), Some("plan-1"));
    let view = wallet.request(&request).await.unwrap();
    assert!(view.periods[0].paid && view.periods[0].credited);
    assert_eq!(
        view.periods[0].receipt.as_ref().unwrap().msg_id,
        receipts[0].msg_id
    );
}

#[tokio::test]
async fn test_periods_credit_uses_the_current_period_after_a_competing_write() {
    Box::pin(confirm_current_period()).await;
}

async fn confirm_current_period() {
    use paykit_lib::records::{ActivityRecord, ActivitySide};
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let mut record = signed_period_payment(&w, &shop, &wallet, &request, january()).await;
    record.receipt = None;
    let target = payee_period_at(&request, january());
    let key = shop.app().record_key(&target).unwrap();
    writes_value(&mut shop, &target, &record.to_json().unwrap()).await;
    seller_wallet.receive(None).await.unwrap();
    let mut winning: serde_json::Value =
        serde_json::from_slice(&record.to_json().unwrap()).unwrap();
    winning["shop_note"] = serde_json::json!("January invoice");
    writes_value(
        &mut seller_wallet,
        &target,
        &serde_json::to_vec(&winning).unwrap(),
    )
    .await;

    let checks = w.rail.proof_checks().len();
    let writes = w.record_writes(&w.bob, &key);
    let entries = w.alice.switch().entries_from(&w.bob_id());
    let id = shop.confirm_payment(&key).await.unwrap().unwrap();
    assert_eq!(w.rail.proof_checks().len(), checks + 2);
    assert_eq!(w.record_writes(&w.bob, &key), writes + 1);
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), entries + 1);
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    let value = held.content.value().unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(value).unwrap()["shop_note"],
        winning["shop_note"]
    );
    let current = ActivityRecord::read(value, &w.bob_id(), ActivitySide::Payee).unwrap();
    let proof = current.proof.unwrap();
    let receipt = current.receipt.unwrap();
    assert_eq!(receipt.message().msg_id, id);
    assert_eq!(
        receipt.message().refs,
        vec![proof.hash(), request.request_hash]
    );
    assert_eq!(receipt.body().billing_period(), Some(&january()));
    assert!(shop.payee_activity().await.unwrap()[0].periods[0].credited);
}

#[tokio::test]
async fn test_recurring_receipt_evidence_as_its_proof_becomes_available() {
    Box::pin(recurring_receipt_evidence_arrives()).await;
}

async fn recurring_receipt_evidence_arrives() {
    use paykit_lib::records::ActivitySide;
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let mut record = signed_period_payment(&w, &shop, &wallet, &request, january()).await;
    let id = record.receipt.as_ref().unwrap().message().msg_id.clone();
    let proof = record.proof.take().unwrap();
    let payee_period = payee_period_at(&request, january());
    let payer_period = payer_period_at(&request, january());
    writes_value(&mut shop, &payee_period, &record.to_json().unwrap()).await;
    record.side = ActivitySide::Payer;
    record.counterparty = w.bob_id();
    writes_value(&mut wallet, &payer_period, &record.to_json().unwrap()).await;

    let payee_before = shop
        .payee_activity()
        .await
        .unwrap()
        .remove(0)
        .periods
        .remove(0);
    assert!(!payee_before.paid && !payee_before.credited && payee_before.receipt.is_none());
    let payer_before = wallet.request(&request).await.unwrap().periods.remove(0);
    assert!(!payer_before.paid && payer_before.credited);
    let payer_receipts = wallet
        .receipts(paykit_sdk::ReceiptsOf::Period(request, january()))
        .await
        .unwrap();
    assert_eq!(payer_receipts.len(), 1);
    assert_eq!(payer_receipts[0].payee, w.bob_id());
    assert_eq!(payer_receipts[0].msg_id, id);
    assert_eq!(
        payer_before.receipt.as_ref().unwrap().msg_id,
        payer_receipts[0].msg_id
    );

    record.proof = Some(proof);
    writes_value(&mut wallet, &payer_period, &record.to_json().unwrap()).await;
    record.side = ActivitySide::Payee;
    record.counterparty = w.alice_id();
    writes_value(&mut shop, &payee_period, &record.to_json().unwrap()).await;
    let payee_after = shop
        .payee_activity()
        .await
        .unwrap()
        .remove(0)
        .periods
        .remove(0);
    let payer_after = wallet.request(&request).await.unwrap().periods.remove(0);
    assert!(payee_after.paid && payee_after.credited);
    assert!(payer_after.paid && payer_after.credited);
    assert_eq!(payee_after.receipt.as_ref().unwrap().payee, w.bob_id());
    assert_eq!(
        payee_after.receipt.as_ref().unwrap().msg_id,
        payer_receipts[0].msg_id
    );
    assert_eq!(payer_after.receipt.as_ref().unwrap().msg_id, id);
}
