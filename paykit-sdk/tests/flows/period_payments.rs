//! Paying a subscription's Billing Periods (Paykit spec 7.2, 8.2; Paykit wire 4.10, 4.11,
//! 5.3): each period on its own obligation, by the user's confirmation or by the app's
//! policy, with `attempted_periods`, the period's proof and receipt in its period record,
//! the payer's cancellation conditioned on the periods' claims, and the hold after a
//! restore.

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_lib::bodies::{PaymentProof, PeriodDetails, RequestDestination};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
use paykit_lib::{AttemptId, Body, PaymentEndpointPayload, PaymentReference};
use paykit_sdk::rail::{Behaviour, RailOutcome};
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitBackup, PaykitConfig,
    PaykitError, Rail, ReceiptsOf, Refusal, RequestDraft, RequestRef, RequestStatus,
};
use pubky_switch_client::app::{
    Collection as SwitchCollection, Initiator, OpBatch, RecordTarget, UserConfirmed,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::clock::TestClock;
use pubky_switch_client::testing::START_MS;
use pubky_switch_core::claims::ClaimStatus;

use crate::common::{btc, endpoint, store, World, HOUR_MS};

const DAY_MS: u64 = 24 * HOUR_MS;

/// The time after which an attempt opened now is orphaned, with a second to spare.
const PAST_ORPHAN_MS: u64 = 4201 * 1000;

/// A monthly request for 1 btc from the start of the world's clock, paid to one invoice of
/// the sending app, its proposal open a day.
fn monthly() -> RequestDraft {
    let mut endpoints = BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt11"),
        PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
    );
    let rule = AnchoredRule::new(START_MS, 1, Unit::Month).unwrap();
    RequestDraft::new(
        btc("1"),
        PaymentReference::new("plan-1").unwrap(),
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(START_MS + DAY_MS)
    .with_recurrence(Recurrence::new(rule, START_MS, None).unwrap())
}

/// The request's first `n` Billing Periods, from January 2026.
fn periods(n: usize) -> Vec<PeriodRef> {
    let rule = AnchoredRule::new(START_MS, 1, Unit::Month).unwrap();
    Recurrence::new(rule, START_MS, None)
        .unwrap()
        .periods(n)
        .unwrap()
        .into_iter()
        .map(PeriodRef::from)
        .collect()
}

fn january() -> PeriodRef {
    periods(1)[0]
}

fn february() -> PeriodRef {
    periods(2)[1]
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

/// Bob's shop sends alice's apps `draft`; `wallet` receives it. Returns the request.
async fn requested(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    draft: RequestDraft,
) -> RequestRef {
    let sent = shop.send_request(w.alice_id(), draft).await.unwrap();
    wallet.receive(None).await.unwrap();
    sent.request.unwrap()
}

/// `wallet` accepts `draft`'s subscription and the shop reads it. Returns the request and
/// the shop's record of it.
async fn subscribed_to(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
    draft: RequestDraft,
) -> (RequestRef, [u8; 32]) {
    let request = requested(w, shop, wallet, draft).await;
    wallet.accept_subscription(&request).await.unwrap();
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    (request, sale)
}

async fn subscribed(w: &World, shop: &mut Paykit, wallet: &mut Paykit) -> (RequestRef, [u8; 32]) {
    subscribed_to(w, shop, wallet, monthly()).await
}

/// `wallet`'s subscription to bob's monthly request, and its first Billing Period.
pub async fn subscription(
    w: &World,
    shop: &mut Paykit,
    wallet: &mut Paykit,
) -> (RequestRef, PeriodRef) {
    (subscribed(w, shop, wallet).await.0, january())
}

fn refusal(error: &PaykitError) -> Refusal {
    error
        .refusal()
        .cloned()
        .unwrap_or_else(|| panic!("{error:?}"))
}

async fn by_user(
    app: &mut Paykit,
    request: &RequestRef,
    period: PeriodRef,
    options: PayOptions,
) -> paykit_sdk::Result<PayOutcome> {
    app.pay_period(request, &period, Initiator::User, options)
        .await
}

async fn by_policy(
    app: &mut Paykit,
    request: &RequestRef,
    period: PeriodRef,
) -> paykit_sdk::Result<PayOutcome> {
    app.pay_period(
        request,
        &period,
        Initiator::Automatic,
        PayOptions::default(),
    )
    .await
}

fn early() -> PayOptions {
    PayOptions::default().with_early_payment_confirmed(UserConfirmed::new())
}

/// The record `app` holds at the payer's `natural` key, read as alice's.
async fn payer_record(w: &World, app: &Paykit, natural: &[u8]) -> Option<ActivityRecord> {
    let collection = Collection::PayerActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        natural,
    );
    let key = app.app().record_key(&target).unwrap();
    let held = app.app().shared_record(&key).await.unwrap()?;
    ActivityRecord::read(held.content.value()?, &w.alice_id(), ActivitySide::Payer).ok()
}

async fn request_record(w: &World, app: &Paykit, request: &RequestRef) -> ActivityRecord {
    let natural = natural_key::request_record(&request.payee, &request.request_hash);
    payer_record(w, app, &natural).await.unwrap()
}

async fn period_record(
    w: &World,
    app: &Paykit,
    request: &RequestRef,
    period: PeriodRef,
) -> Option<ActivityRecord> {
    let natural =
        natural_key::period_record(&request.payee, &request.request_hash, period.start_secs());
    payer_record(w, app, &natural).await
}

/// The proof `record` holds, with its body.
fn proof_of(record: &ActivityRecord) -> (&StoredMessage, &PaymentProof) {
    let stored = record.proof.as_ref().unwrap();
    let Body::PaymentProof(proof) = stored.body() else {
        panic!("not a proof");
    };
    (stored, proof)
}

#[tokio::test]
async fn test_period_payments_pays_a_period_by_the_requests_endpoint() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // The request record lists the period, and the period record holds the proof, which
    // names the period and only the request (Paykit wire 4.10, 5.3).
    let listed = request_record(&w, &wallet, &request).await;
    assert_eq!(listed.attempted_periods, [january().start_secs()]);
    assert!(listed.proof.is_none());
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    let (stored, proof) = proof_of(&record);
    assert_eq!(proof.billing_period, Some(january()));
    assert!(proof.allowance_id.is_none());
    assert_eq!(stored.message().refs, [request.request_hash]);
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Subscribed);
    assert!(view.periods[0].paid, "{view:?}");
    let payment = &wallet.payments().await.unwrap()[0];
    assert_eq!(payment.billing_period, Some(january()));
    // The payee credits the period, not the request.
    shop.receive(None).await.unwrap();
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert_eq!(sale.status, PayeeStatus::Accepted);
    assert!(sale.periods[0].paid, "{sale:?}");
    // The record shows it paid: the period isn't paid again.
    let again = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&again), Refusal::AlreadyPaid);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_period_payments_refused_before_acceptance_and_for_what_isnt_a_period() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = requested(&w, &mut shop, &mut wallet, monthly()).await;
    let refused = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::NotSubscribed);
    wallet.accept_subscription(&request).await.unwrap();
    let mut off = january();
    off.ends_at += 1;
    let refused = by_user(&mut wallet, &request, off, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
    // A one-time request is paid with pay_request, and a recurring one never as a whole.
    let one_time =
        crate::common::request_from(&mut shop, w.alice_id(), crate::common::draft("1", START_MS))
            .await;
    wallet.receive(None).await.unwrap();
    let refused = by_user(&mut wallet, &one_time, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Recurring);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_period_payments_latest_details_paid_by_and_named_in_the_proof() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1first", now + DAY_MS))
        .await
        .unwrap();
    w.clock.advance_ms(1_000);
    let second = shop
        .send_period_details(&sale, details(january(), "lnbc1second", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    let held = record.period_details.as_ref().unwrap();
    assert_eq!(held.message().msg_id, second);
    let (stored, _) = proof_of(&record);
    assert_eq!(stored.message().refs, [request.request_hash, held.hash()]);
    // The payee credits the period by the details it sent.
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap()[0].periods[0].paid);
}

#[tokio::test]
async fn test_period_payments_earlier_details_chosen_point_the_record_back() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    let first = shop
        .send_period_details(&sale, details(january(), "lnbc1first", now + DAY_MS))
        .await
        .unwrap();
    w.clock.advance_ms(1_000);
    let second = shop
        .send_period_details(&sale, details(january(), "lnbc1second", now + 2 * DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    // Details the period record doesn't hold aren't paid by (Paykit spec 8.2).
    let refused = by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_period_details("not-held"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NoDestination(_)),
        "{refused:?}"
    );
    by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_period_details(first.clone()),
    )
    .await
    .unwrap();
    // The proof's write points `period_details` back at the details it names, the later
    // ones kept as earlier details, and the record reads (Paykit wire 5.3).
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert_eq!(
        record.period_details.as_ref().unwrap().message().msg_id,
        first
    );
    assert_eq!(record.earlier_details.len(), 1);
    assert_eq!(record.earlier_details[0].message().msg_id, second);
    let (stored, _) = proof_of(&record);
    assert_eq!(
        stored.message().refs[1],
        record.period_details.as_ref().unwrap().hash()
    );
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap()[0].periods[0].paid);
}

#[tokio::test]
async fn test_period_payments_expired_details_unused_the_request_paid_by() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    let expiring = shop
        .send_period_details(&sale, details(january(), "lnbc1short", now + HOUR_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(HOUR_MS);
    wallet.receive(None).await.unwrap();
    let refused = by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_period_details(expiring),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NoDestination(_)),
        "{refused:?}"
    );
    by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert_eq!(proof_of(&record).0.message().refs, [request.request_hash]);
}

#[tokio::test]
async fn test_period_payments_per_period_endpoints_wait_for_the_periods_details() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed_to(
        &w,
        &mut shop,
        &mut wallet,
        monthly().with_per_period_endpoints(),
    )
    .await;
    w.clock.set_ms(february().starts_at);
    wallet.receive(None).await.unwrap();
    // A later period waits for period details with its destination (Paykit spec 8.2).
    let refused = by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NoDestination(_)),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    // The first is the request's.
    by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(february(), "lnbc1feb", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap();
    let record = period_record(&w, &wallet, &request, february())
        .await
        .unwrap();
    assert_eq!(proof_of(&record).0.message().refs.len(), 2);
}

#[tokio::test]
async fn test_period_payments_before_its_start_only_by_the_users_early_confirmation() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let refused = by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::PeriodNotStarted);
    let refused = by_policy(&mut wallet, &request, february())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::PeriodNotStarted);
    // A payment by policy carries no user's confirmation.
    let refused = wallet
        .pay_period(&request, &february(), Initiator::Automatic, early())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_attempted_periods_drop_finished_and_keep_open_ones() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let all = periods(3);
    by_user(&mut wallet, &request, all[0], PayOptions::default())
        .await
        .unwrap();
    w.rail.next(Behaviour::Hang);
    let pending = by_user(&mut wallet, &request, all[1], early())
        .await
        .unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }), "{pending:?}");
    // January finished: the write that adds February drops it.
    let record = request_record(&w, &wallet, &request).await;
    assert_eq!(record.attempted_periods, [all[1].start_secs()]);
    by_user(&mut wallet, &request, all[2], early())
        .await
        .unwrap();
    // February's attempt is open: it stays listed.
    let record = request_record(&w, &wallet, &request).await;
    assert_eq!(
        record.attempted_periods,
        [all[1].start_secs(), all[2].start_secs()]
    );
    // An attempt on a listed period writes no request record.
    let before = request_record(&w, &wallet, &request).await;
    w.rail.settle(&pending.attempt_id(), false);
    wallet.resume_payments().await.unwrap();
    by_user(&mut wallet, &request, all[1], early())
        .await
        .unwrap();
    assert_eq!(
        request_record(&w, &wallet, &request).await.msg_ids,
        before.msg_ids
    );
}

#[tokio::test]
async fn test_period_payments_sixteen_open_periods_refuse_another() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let all = periods(17);
    for period in &all[..16] {
        w.rail.next(Behaviour::Hang);
        let outcome = by_user(&mut wallet, &request, *period, early())
            .await
            .unwrap();
        assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    }
    assert_eq!(
        request_record(&w, &wallet, &request)
            .await
            .attempted_periods
            .len(),
        16
    );
    let refused = by_user(&mut wallet, &request, all[16], early())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::AttemptedPeriodsFull);
    assert_eq!(w.rail.all_executions().len(), 16);
}

#[tokio::test]
async fn test_period_payments_payer_cancellation_ends_the_subscription() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    // A paid period doesn't hold the cancellation back.
    wallet.cancel_request(&request, None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Cancelled
    );
    let refused = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Cancelled);
}

#[tokio::test]
async fn test_period_payments_cancellation_refused_while_a_period_is_being_paid() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let pending = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    // The phone's cancellation reads the period's claim open (Paykit spec 8.2, "Ending").
    phone.receive(None).await.unwrap();
    let refused = phone.cancel_request(&request, None).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::PaymentInProgress);
    // Once the period is paid, the cancellation goes.
    w.rail.settle(&pending.attempt_id(), true);
    wallet.resume_payments().await.unwrap();
    phone.receive(None).await.unwrap();
    phone.cancel_request(&request, None).await.unwrap();
    shop.receive(None).await.unwrap();
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert_eq!(sale.status, PayeeStatus::Cancelled);
    assert!(sale.periods[0].paid);
}

#[tokio::test]
async fn test_period_payments_payee_cancellation_stops_every_period() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    shop.cancel_sent_request(&sale, None).await.unwrap();
    wallet.receive(None).await.unwrap();
    for by in [Initiator::User, Initiator::Automatic] {
        w.clock.set_ms(february().starts_at);
        let refused = wallet
            .pay_period(&request, &february(), by, PayOptions::default())
            .await
            .unwrap_err();
        assert_eq!(refusal(&refused), Refusal::Cancelled, "{by:?}");
    }
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_period_payments_receipt_goes_into_the_period_record() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let period = shop.payee_activity().await.unwrap()[0].periods[0].record;
    shop.confirm_payment(&period).await.unwrap().unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    let receipt = view.periods[0].receipt.as_ref().unwrap();
    assert_eq!(receipt.billing_period, Some(january()));
    assert!(view.receipt.is_none(), "{view:?}");
    let held = wallet
        .receipts(ReceiptsOf::Period(request, january()))
        .await
        .unwrap();
    assert_eq!(held.len(), 1);
    assert!(wallet
        .receipts(ReceiptsOf::Period(request, february()))
        .await
        .unwrap()
        .is_empty());
    assert!(wallet.payments().await.unwrap()[0].receipt.is_some());
}

/// A payment of 1 btc at the fake rail, outside the wallet, and alice's wallet's proof of
/// it to the shop's invoice for `period`, signed and not sent.
async fn outside_proof(
    w: &World,
    shop: &Paykit,
    request: &RequestRef,
    period: PeriodRef,
) -> pubky_switch_core::message::MessagePlaintext {
    let attempt = AttemptId::from_bytes([9; 16]);
    let RailOutcome::Paid(paid) = w
        .rail
        .pay_outside(
            endpoint("btc-lightning-bolt11"),
            PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
            btc("1"),
            attempt,
        )
        .await
        .unwrap()
    else {
        panic!("not paid");
    };
    let body = PaymentProof {
        attempt_id: attempt,
        billing_period: Some(period),
        payment_endpoint_identifier: endpoint("btc-lightning-bolt11"),
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
    w.sign_as(
        &w.alice,
        "wallet",
        &w.bob,
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    )
}

#[tokio::test]
async fn test_period_payments_receipt_before_its_proof_kept_then_placed() {
    // The shop receipts a proof of January that the wallet's records don't hold yet (made
    // by another own app whose outcome this one hasn't read): the receipt is kept as a
    // message before its record, and placed once the period record holds the proof (Paykit
    // spec 8.1, "Messages before their record").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let proof = outside_proof(&w, &shop, &request, january()).await;
    w.deliver(&w.alice, "wallet", &w.bob, &[shop.app_id()], &proof);
    shop.receive(None).await.unwrap();
    let period = shop.payee_activity().await.unwrap()[0].periods[0].record;
    shop.confirm_payment(&period).await.unwrap().unwrap();
    wallet.receive(None).await.unwrap();
    assert!(period_record(&w, &wallet, &request, january())
        .await
        .is_none());
    // The other own app's outcome: the period record with the proof.
    let keys = w.alice.identity().app_keys("wallet");
    let cert = w.alice.switch().app(&keys.app_id_bytes()).cert.unwrap();
    let stored = StoredMessage::new(w.alice_id(), cert.signed(), proof, None, &w.bob_id()).unwrap();
    let mut record = ActivityRecord::new(ActivitySide::Payer, w.bob_id());
    record.request_hash = Some(request.request_hash);
    record.billing_period = Some(january());
    record.msg_ids = vec![stored.message().msg_id.clone()];
    record.proof = Some(stored);
    let collection = Collection::PayerActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::period_record(
            &request.payee,
            &request.request_hash,
            january().start_secs(),
        ),
    );
    let op = wallet
        .app_mut()
        .record_op(&target, Some(&record.to_json().unwrap()), Vec::new())
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    let receipt = view.periods[0].receipt.as_ref();
    assert_eq!(
        receipt.map(|r| r.billing_period),
        Some(Some(january())),
        "{view:?}"
    );
}

#[tokio::test]
async fn test_period_payments_by_policy_pays_when_every_automatic_rule_holds() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let outcome = by_policy(&mut wallet, &request, january()).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    // No Allowance: its proof carries no `allowance_id` (Paykit spec 8.2).
    assert!(proof_of(&record).1.allowance_id.is_none());
}

#[tokio::test]
async fn test_period_payments_by_policy_carries_no_users_confirmation() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    for options in [
        PayOptions::default().with_possibly_cancelled_confirmed(UserConfirmed::new()),
        PayOptions::default().with_unconfirmed_identity_confirmed(UserConfirmed::new()),
        PayOptions::default().with_mode(paykit_sdk::PaymentMode::Public(UserConfirmed::new())),
    ] {
        let refused = wallet
            .pay_period(&request, &january(), Initiator::Automatic, options)
            .await
            .unwrap_err();
        assert!(
            matches!(refusal(&refused), Refusal::Usage(_)),
            "{refused:?}"
        );
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_period_payments_by_policy_reads_the_deny_list_fresh() {
    // Bob is blocked by another own app after the wallet read the lists: the user's payment
    // judges the copy read less than 10 minutes ago, the payment by policy reads the list
    // again and pays nothing (Paykit spec 8.1).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut manager = w
        .join(
            &w.alice,
            "manager",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
        )
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    manager.block(&w.bob_id()).await.unwrap();
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Blocked { .. }),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    // A blocked payment by policy isn't passed by the user's confirmation of it either.
    let Refusal::Blocked { payment } = refusal(&refused) else {
        unreachable!()
    };
    assert_eq!(payment.period, Some(january()));
    let refused = wallet
        .pay_period(
            &request,
            &january(),
            Initiator::Automatic,
            PayOptions::default().with_blocked_confirmed(*payment),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
    manager.unblock(&w.bob_id()).await.unwrap();
    let outcome = by_policy(&mut wallet, &request, january()).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_by_policy_held_while_the_clocks_disagree() {
    // The wallet's device clock runs 10 minutes behind the service: a payment by policy
    // isn't attempted, a user's is (Paykit spec 7.2).
    let w = World::new();
    let mut shop = w.shop().await;
    let base = store();
    let wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let id = wallet.app_id();
    drop(wallet);
    let behind = Arc::new(TestClock::new(w.clock.now_ms() - 10 * 60_000));
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), id, behind)
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NotNow(_)),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_by_policy_never_pays_over_a_doubtful_claim() {
    // The phone's attempt on January turned `unknown`: the user may pay over that doubt
    // once shown it; a payment by policy never does (Paykit spec 7.1).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let first = by_user(&mut phone, &request, january(), PayOptions::default())
        .await
        .unwrap()
        .attempt_id();
    let obligation = phone.payments().await.unwrap()[0].obligation;
    w.clock.advance_ms(PAST_ORPHAN_MS);
    wallet.receive(None).await.unwrap();
    wallet
        .resolve_attempt(
            &obligation,
            first,
            paykit_sdk::Resolution::Unknown,
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    let Refusal::MayBePaid { doubt, .. } = refusal(&refused) else {
        panic!("{refused:?}");
    };
    let doubt = *doubt.unwrap();
    let refused = wallet
        .pay_period(
            &request,
            &january(),
            Initiator::Automatic,
            PayOptions::default().with_may_be_paid_confirmed(doubt.clone()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    let outcome = by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_may_be_paid_confirmed(doubt),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_by_policy_pays_only_the_amount_requested() {
    // The period's details name an invoice for another amount: the user may pay it on
    // seeing what it transfers; a payment by policy never does (Paykit spec 7.2 step 1).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(
        &sale,
        details(january(), "lnbc1other?amount=2", now + DAY_MS),
    )
    .await
    .unwrap();
    wallet.receive(None).await.unwrap();
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Transfer { .. }),
        "{refused:?}"
    );
    let Refusal::Transfer { quote, .. } = refusal(&refused) else {
        unreachable!()
    };
    let refused = wallet
        .pay_period(
            &request,
            &january(),
            Initiator::Automatic,
            PayOptions::default().with_transfer_confirmed(*quote.clone()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_transfer_confirmed(*quote),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

async fn restored(w: &World, backup: &PaykitBackup) -> Paykit {
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap(),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    app.receive(None).await.unwrap();
    app
}

#[tokio::test]
async fn test_period_payments_subscription_held_after_a_restore_until_the_user_confirms() {
    // The recurring branch of `restore_stale_backup_cancellation_stop`: no period is paid
    // after a restore until the user has seen the payee may have cancelled it, and never
    // by policy meanwhile (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    let refused = by_user(&mut app, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::HeldAfterRestore);
    let refused = by_policy(&mut app, &request, january()).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::HeldAfterRestore);
    assert!(w.rail.all_executions().is_empty());
    let confirmed = PayOptions::default().with_possibly_cancelled_confirmed(UserConfirmed::new());
    by_user(&mut app, &request, january(), confirmed)
        .await
        .unwrap();
    // The confirmation ended the hold of the request: its next period is paid by policy.
    w.clock.set_ms(february().starts_at);
    let outcome = by_policy(&mut app, &request, february()).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_cancellation_after_the_backup_stops_the_restored_app() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    // The payee cancels after the backup, and the device that held it is lost.
    shop.cancel_sent_request(&sale, None).await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    // The restore asked bob to re-send; his cancellation stops every period for good.
    shop.receive(None).await.unwrap();
    app.receive(None).await.unwrap();
    let confirmed = PayOptions::default().with_possibly_cancelled_confirmed(UserConfirmed::new());
    let refused = by_user(&mut app, &request, january(), confirmed)
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Cancelled);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_period_payments_reconcile_finds_a_period_paid_after_the_backup() {
    // February is paid after the backup and the device lost before its outcome: the
    // restored app finds the attempt on the period the request record lists, from the
    // payment method's history, and sends its proof (Paykit spec 7.3, 8.2).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    w.rail.next(Behaviour::Hang);
    let attempt = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap()
        .attempt_id();
    w.rail.settle(&attempt, true);
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    assert!(report.doubtful.is_empty(), "{report:?}");
    assert!(
        report
            .resolved
            .iter()
            .any(|o| matches!(o, PayOutcome::Paid { .. })),
        "{report:?}"
    );
    let record = period_record(&w, &app, &request, february()).await.unwrap();
    assert_eq!(proof_of(&record).1.billing_period, Some(february()));
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap()[0].periods[0].paid);
    assert_eq!(w.rail.executions(&attempt), 1);
}

#[tokio::test]
async fn test_period_payments_reconcile_leaves_a_period_paid_by_details_doubtful() {
    // A period paid to its period details' destination can't be told from the records: the
    // attempt stays doubtful, for the user.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed_to(
        &w,
        &mut shop,
        &mut wallet,
        monthly().with_per_period_endpoints(),
    )
    .await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(february(), "lnbc1feb", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    w.rail.next(Behaviour::Hang);
    let attempt = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap()
        .attempt_id();
    w.rail.settle(&attempt, true);
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    assert_eq!(report.doubtful.len(), 1, "{report:?}");
    assert_eq!(report.doubtful[0].attempts, [attempt]);
    assert_eq!(report.doubtful[0].request_hash, Some(request.request_hash));
    let claim = app
        .app_mut()
        .read_claim(&report.doubtful[0].obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Attempting);
}
