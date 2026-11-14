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
use pubky_switch_client::store::StateStore;
use pubky_switch_client::testing::START_MS;
use pubky_switch_core::claims::{ClaimStatus, ClaimType};
use pubky_switch_core::ops::{Condition, OpKind};

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

/// The conditions of every `attempt` alice's switch applied on the claim of `obligation`.
fn attempt_conditions(w: &World, obligation: &[u8; 32]) -> Vec<Vec<Condition>> {
    w.alice
        .switch()
        .applied()
        .iter()
        .filter(|op| {
            matches!(&op.body().kind, OpKind::Claim(fields)
                if fields.kind == ClaimType::Attempt && fields.claim_key == obligation.to_vec())
        })
        .map(|op| op.body().conditions.clone())
        .collect()
}

/// The conditions of the last `record.put` alice's switch applied to the payer's request
/// record of `request`, as `app` keys it.
fn last_request_put(w: &World, app: &Paykit, request: &RequestRef) -> Vec<Condition> {
    let collection = Collection::PayerActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    );
    let key = app.app().record_key(&target).unwrap();
    w.alice
        .switch()
        .applied()
        .iter()
        .rev()
        .find(|op| matches!(&op.body().kind, OpKind::RecordPut(put) if put.key == key))
        .map(|op| op.body().conditions.clone())
        .unwrap()
}

/// The claim key of `app`'s payment of `period`.
async fn period_obligation(app: &Paykit, period: PeriodRef) -> [u8; 32] {
    app.payments()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.billing_period == Some(period))
        .unwrap()
        .obligation
}

fn starts_at(conditions: &[Condition]) -> Option<u64> {
    conditions.iter().find_map(|c| match c {
        Condition::ServerTimeIn { start, end: None } => *start,
        _ => None,
    })
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
    // The attempt is conditioned on the request record and applied only from the period's
    // start, on the service's clock (Paykit spec 8.2, "Not before its start").
    let conditions = attempt_conditions(&w, &payment.obligation);
    assert_eq!(conditions.len(), 1);
    assert_eq!(starts_at(&conditions[0]), Some(january().starts_at));
    assert!(conditions[0]
        .iter()
        .any(|c| matches!(c, Condition::RecordVersion { .. })));
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
        .unwrap()
        .msg_id;
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
        .unwrap()
        .msg_id;
    w.clock.advance_ms(1_000);
    let second = shop
        .send_period_details(&sale, details(january(), "lnbc1second", now + 2 * DAY_MS))
        .await
        .unwrap()
        .msg_id;
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
        .unwrap()
        .msg_id;
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
    // A later period waits for valid period details with its destination (Paykit spec 8.2):
    // none yet, then only expired ones.
    let refused = by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::WaitingForPeriodDetails);
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(february(), "lnbc1short", now + HOUR_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(HOUR_MS);
    let refused = by_policy(&mut wallet, &request, february())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::WaitingForPeriodDetails);
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
    // Applied from the time the user confirmed, the latest service time verified then.
    let obligation = period_obligation(&wallet, february()).await;
    let start = starts_at(&attempt_conditions(&w, &obligation)[0]).unwrap();
    assert!(start < february().starts_at && start >= START_MS, "{start}");
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
    // The drop is conditioned on January's claim at the step read (Paykit wire 5.3).
    let january_claim = period_obligation(&wallet, all[0]).await;
    assert!(last_request_put(&w, &wallet, &request)
        .iter()
        .any(|c| matches!(
            c,
            Condition::ClaimStep { claim_key, step } if *claim_key == january_claim && *step == 2
        )));
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
    let before = request_version(&wallet, &request).await;
    w.rail.settle(&pending.attempt_id(), false);
    wallet.resume_payments().await.unwrap();
    by_user(&mut wallet, &request, all[1], early())
        .await
        .unwrap();
    assert_eq!(request_version(&wallet, &request).await, before);
}

/// The version of the payer's request record of `request` that `app` holds.
async fn request_version(
    app: &Paykit,
    request: &RequestRef,
) -> pubky_switch_core::ops::RecordVersion {
    let collection = Collection::PayerActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    );
    let key = app.app().record_key(&target).unwrap();
    app.app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .version
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
    // It is conditioned on the listed period's claim (Paykit spec 8.2, "Ending").
    let january_claim = period_obligation(&wallet, january()).await;
    assert!(last_request_put(&w, &wallet, &request)
        .iter()
        .any(|c| matches!(
            c,
            Condition::ClaimStep { claim_key, .. } if *claim_key == january_claim
        )));
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
    // The record shows the period paid, whatever its claim shows: it isn't paid again
    // (Paykit spec 7.2, "The record shows it paid").
    let refused = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::AlreadyPaid);
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

/// A wallet on an idempotent rail without history pays January of bob's subscription, the
/// dispatch left in flight; then `between` runs with the shop's record of the sale, and the
/// wallet resumes a minute later. Returns how many times the rail was called.
async fn resumed_period_repeat(w: &World, between: impl AsyncFnOnce(&mut Paykit, [u8; 32])) -> u64 {
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let mut shop = w.shop().await;
    let mut wallet = w
        .wallet_with("wallet", store(), crate::common::day_window())
        .await;
    let (request, sale) = subscribed(w, &mut shop, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    let pending = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }), "{pending:?}");
    between(&mut shop, sale).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    wallet.resume_payments().await.unwrap();
    w.rail.pay_calls(&pending.attempt_id())
}

#[tokio::test]
async fn test_period_payments_resume_repeats_an_idempotent_dispatch_of_a_period() {
    let w = World::new();
    assert_eq!(resumed_period_repeat(&w, async |_, _| {}).await, 2);
}

#[tokio::test]
async fn test_period_payments_resume_repeats_no_dispatch_the_payee_cancelled_since() {
    // The payee cancels the subscription while January's dispatch is in flight: the repeat
    // isn't made, the attempt staying "may be paid" (Paykit spec 7.2, 8.2).
    let w = World::new();
    let calls = resumed_period_repeat(&w, async |shop, sale| {
        shop.cancel_sent_request(&sale, None).await.unwrap();
    })
    .await;
    assert_eq!(calls, 1);
}

/// `app` writes `record` at the payer's `natural` key, built on the version it holds.
async fn write_payer_record(app: &mut Paykit, natural: &[u8], record: &ActivityRecord) {
    let collection = Collection::PayerActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        natural,
    );
    let op = app
        .app_mut()
        .record_op(&target, Some(&record.to_json().unwrap()), Vec::new())
        .await
        .unwrap();
    app.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_period_payments_request_accepted_under_an_allowance_paid_only_by_hand() {
    // The request record names the Allowance it was accepted under: no app pays it by its
    // own policy, and the user's payment goes ahead (Paykit spec 8.3, "One Allowance per
    // request").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let mut record = request_record(&w, &wallet, &request).await;
    record.allowance_id =
        Some(paykit_lib::AllowanceId::parse("0b4f9c1e-8d2a-4e6b-9f3c-5a7d1e2b8c40").unwrap());
    let natural = natural_key::request_record(&request.payee, &request.request_hash);
    write_payer_record(&mut wallet, &natural, &record).await;
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::ManualOnly);
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // The request record keeps the Allowance it names.
    assert!(request_record(&w, &wallet, &request)
        .await
        .allowance_id
        .is_some());
}

#[tokio::test]
async fn test_period_payments_period_marked_manual_only_paid_only_by_hand() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1jan", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let mut record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    record.manual_only = true;
    let natural = natural_key::period_record(
        &request.payee,
        &request.request_hash,
        january().start_secs(),
    );
    write_payer_record(&mut wallet, &natural, &record).await;
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::ManualOnly);
    // February isn't marked: paid by policy once it starts.
    w.clock.set_ms(february().starts_at);
    let outcome = by_policy(&mut wallet, &request, february()).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_two_due_periods_are_two_rail_payments() {
    // One attempt is one payment of the method, for one occurrence: two due periods are
    // paid by two payments, never one transaction for both (one settlement, one occurrence).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    w.clock.set_ms(february().starts_at);
    // January's automatic window ended when February started: the user pays it.
    let first = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    let second = by_policy(&mut wallet, &request, february()).await.unwrap();
    assert_ne!(first.attempt_id(), second.attempt_id());
    let executions = w.rail.all_executions();
    assert_eq!(executions.len(), 2, "{executions:?}");
    assert!(executions.values().all(|n| *n == 1));
    assert_ne!(
        period_obligation(&wallet, january()).await,
        period_obligation(&wallet, february()).await
    );
    shop.receive(None).await.unwrap();
    let sale = &shop.payee_activity().await.unwrap()[0];
    assert!(
        sale.periods.iter().all(|p| p.paid) && sale.periods.len() == 2,
        "{sale:?}"
    );
}

#[tokio::test]
async fn test_period_payments_cancellation_refused_over_an_unknown_attempt_until_released() {
    // January's attempt by the phone turned `unknown`: the period stays listed and the
    // cancellation is refused as may be paid (Paykit spec 8.2, "Ending"). Once its owner
    // reports it failed (Switch spec 10.2 row 5), the cancellation goes.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let attempt = by_user(&mut phone, &request, january(), PayOptions::default())
        .await
        .unwrap()
        .attempt_id();
    let obligation = period_obligation(&phone, january()).await;
    w.clock.advance_ms(PAST_ORPHAN_MS);
    wallet.receive(None).await.unwrap();
    let confirmed = UserConfirmed::new();
    wallet
        .resolve_attempt(
            &obligation,
            attempt,
            paykit_sdk::Resolution::Unknown,
            confirmed,
        )
        .await
        .unwrap();
    let refused = wallet.cancel_request(&request, None).await.unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::MayBePaid { .. }),
        "{refused:?}"
    );
    // A period with an `unknown` attempt stays listed: the record still fences it.
    assert_eq!(
        request_record(&w, &wallet, &request)
            .await
            .attempted_periods,
        [january().start_secs()]
    );
    // Nor does a write that adds another period drop it.
    w.clock.set_ms(february().starts_at);
    by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap();
    assert!(request_record(&w, &wallet, &request)
        .await
        .attempted_periods
        .contains(&january().start_secs()));
    w.rail.settle(&attempt, false);
    phone.resume_payments().await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.cancel_request(&request, None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
}

#[tokio::test]
async fn test_period_payments_by_policy_fails_over_a_manual_only_mark_written_since() {
    // The phone marks January manual-only after the wallet last read the period record: the
    // wallet's payment by policy is conditioned on the version it read, so its batch fails
    // and nothing is paid; read again, the mark refuses it (Paykit spec 8.3).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1jan", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let mut record = period_record(&w, &phone, &request, january())
        .await
        .unwrap();
    record.manual_only = true;
    let natural = natural_key::period_record(
        &request.payee,
        &request.request_hash,
        january().start_secs(),
    );
    write_payer_record(&mut phone, &natural, &record).await;
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert!(matches!(refused, PaykitError::Conflict(_)), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
    wallet.receive(None).await.unwrap();
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::ManualOnly);
}

#[tokio::test]
async fn test_period_payments_outcome_rebuilds_a_deleted_period_record_with_its_details() {
    // January is paid by period details; its period record is deleted before the outcome:
    // the outcome writes it again, the details the proof names placed before the proof, so
    // the record binds (Paykit wire 5.3).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1jan", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let pending = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
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
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    w.rail.settle(&pending.attempt_id(), true);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, PayOutcome::Paid { .. })),
        "{outcomes:?}"
    );
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    let (stored, _) = proof_of(&record);
    assert_eq!(
        stored.message().refs[1],
        record.period_details.as_ref().unwrap().hash()
    );
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap()[0].periods[0].paid);
}

/// A store that serves every journal entry as `tamper` rewrites it, once set: what a damaged
/// or foreign store would hand back.
#[derive(Default)]
struct TamperedJournal {
    inner: pubky_switch_client::store::MemoryStore,
    #[allow(clippy::type_complexity)]
    tamper: std::sync::Mutex<Option<Box<dyn Fn(&mut serde_json::Value) + Send + Sync>>>,
}

impl pubky_switch_client::store::StateStore for TamperedJournal {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        let value = self.inner.get(ns, key)?;
        let tamper = self.tamper.lock().unwrap();
        match (value, tamper.as_ref()) {
            (Some(bytes), Some(tamper)) if key.starts_with(b"p/j/") => {
                let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                tamper(&mut value["d"]);
                Ok(Some(serde_json::to_vec(&value).unwrap()))
            }
            (value, _) => Ok(value),
        }
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_switch_client::store::KeyValue>, pubky_switch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        self.inner.commit(batch)
    }
}

/// The wallet pays January on a store whose journal reads are then rewritten by `tamper`,
/// and receives the shop's receipt of the payment. Returns whether the receipt was placed.
async fn receipt_over_a_tampered_entry(tamper: fn(&mut serde_json::Value)) -> bool {
    let w = World::new();
    let mut shop = w.shop().await;
    let store = Arc::new(TamperedJournal::default());
    let mut wallet = w
        .wallet_on(
            "wallet",
            Arc::clone(&store) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let period = shop.payee_activity().await.unwrap()[0].periods[0].record;
    shop.confirm_payment(&period).await.unwrap().unwrap();
    *store.tamper.lock().unwrap() = Some(Box::new(tamper));
    let _ = wallet.receive(None).await;
    *store.tamper.lock().unwrap() = None;
    wallet.request(&request).await.unwrap().periods[0]
        .receipt
        .is_some()
}

#[tokio::test]
async fn test_period_payments_receipt_parked_over_an_entry_of_another_obligation() {
    // The receipt's attempt is journaled with another claim key than its period's: a corrupt
    // store, which parks the receipt rather than settle a claim through it (state read back
    // as a restore reads it).
    assert!(
        receipt_over_a_tampered_entry(|_| {}).await,
        "an entry that reads places the receipt"
    );
    assert!(
        !receipt_over_a_tampered_entry(|entry| {
            entry["obligation"] = serde_json::json!(pubky_switch_core::enc::b64(&[7; 32]));
        })
        .await
    );
}

#[tokio::test]
async fn test_period_payments_receipt_parked_over_an_entry_naming_another_attempt() {
    assert!(
        !receipt_over_a_tampered_entry(|entry| {
            entry["attempt_id"] = serde_json::json!(AttemptId::from_bytes([5; 16]).to_text());
        })
        .await
    );
}

/// A rewrite of one entry of a backup's Paykit layer: its key and its value.
type Tamper = dyn Fn(&mut Vec<u8>, &mut serde_json::Value);

/// A backup of the wallet with January's payment in flight, and Paykit's layer of it.
async fn backup_paying_january(w: &World) -> (serde_json::Value, [u8; 16]) {
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(w, &mut shop, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    (serde_json::from_slice(&bytes).unwrap(), wallet.app_id())
}

/// Whether a restore takes the backup `w`'s wallet took paying January, its Paykit layer's
/// entries under keys starting with `prefix` rewritten by `tamper`.
async fn restore_takes(
    w: &World,
    backup: &serde_json::Value,
    prefix: &[u8],
    tamper: impl Fn(&mut Vec<u8>, &mut serde_json::Value),
) -> bool {
    let data = backup["protocol"]["data"].as_str().unwrap();
    let bytes = pubky_switch_core::enc::unb64(data).unwrap();
    let mut state: Vec<(Vec<u8>, Vec<u8>)> = pubky_switch_client::app::decode_local_state(&bytes)
        .unwrap()
        .into_iter()
        .collect();
    let mut found = false;
    for (key, value) in &mut state {
        if key.starts_with(prefix) {
            found = true;
            let mut json: serde_json::Value = serde_json::from_slice(value).unwrap();
            tamper(key, &mut json["d"]);
            *value = serde_json::to_vec(&json).unwrap();
        }
    }
    assert!(found, "nothing under the prefix");
    let mut out = backup.clone();
    out["protocol"]["data"] = serde_json::Value::String(pubky_switch_core::enc::b64(
        &pubky_switch_client::app::encode_local_state(&state),
    ));
    let backup = PaykitBackup::from_bytes(&serde_json::to_vec(&out).unwrap()).unwrap();
    Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .is_ok()
}

#[tokio::test]
async fn test_period_payments_restore_refuses_a_period_entry_a_run_cant_write() {
    // A period's journal entry naming another period's claim key, owing an acceptance, at
    // its request record's key, or with an empty period: no run writes one, and the
    // restore refuses the backup.
    let w = World::new();
    let (backup, _) = backup_paying_january(&w).await;
    assert!(restore_takes(&w, &backup, b"j/", |_, _| {}).await);
    let request_key = |entry: &serde_json::Value| {
        let payee =
            pubky_switch_core::enc::unb64_array::<32>(entry["payee"].as_str().unwrap()).unwrap();
        let hash =
            pubky_switch_core::enc::unb64_array::<32>(entry["request_hash"].as_str().unwrap())
                .unwrap();
        pubky_switch_core::enc::b64(&natural_key::request_record(&payee, &hash))
    };
    let refused: [Box<Tamper>; 4] = [
        Box::new(|_, e| e["obligation"] = serde_json::json!(pubky_switch_core::enc::b64(&[7; 32]))),
        Box::new(|_, e| e["acceptance_owed"] = serde_json::json!(true)),
        Box::new(move |_, e| e["natural_key"] = serde_json::json!(request_key(e))),
        Box::new(|_, e| e["billing_period"]["ends_at"] = e["billing_period"]["starts_at"].clone()),
    ];
    for tamper in refused {
        assert!(!restore_takes(&w, &backup, b"j/", tamper).await);
    }
}

#[tokio::test]
async fn test_period_payments_restore_refuses_a_kept_receipt_under_another_record() {
    // A receipt kept before its record is bound to the request record its key names: one
    // kept under another record's key is refused.
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
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    let backup: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(restore_takes(&w, &backup, b"pr/", |_, _| {}).await);
    let other = pubky_switch_core::enc::b64(&[9; 32]);
    assert!(
        !restore_takes(&w, &backup, b"pr/", move |key, _| {
            let text = String::from_utf8(key.clone()).unwrap();
            let parts: Vec<&str> = text.split('/').collect();
            *key = format!("{}/{}/{}/{}", parts[0], parts[1], other, parts[3]).into_bytes();
        })
        .await
    );
}

/// A weekly request for 1 btc from the start of the world's clock, paid to one invoice of
/// the sending app.
fn weekly() -> RequestDraft {
    let rule = AnchoredRule::new(START_MS, 1, Unit::Week).unwrap();
    let mut draft = monthly();
    draft.recurrence = Some(Recurrence::new(rule, START_MS, None).unwrap());
    draft
}

/// The weekly request's first `n` Billing Periods.
fn weeks(n: usize) -> Vec<PeriodRef> {
    let rule = AnchoredRule::new(START_MS, 1, Unit::Week).unwrap();
    Recurrence::new(rule, START_MS, None)
        .unwrap()
        .periods(n)
        .unwrap()
        .into_iter()
        .map(PeriodRef::from)
        .collect()
}

#[tokio::test]
async fn test_period_payments_period_past_the_details_window_paid_by_the_request() {
    // Details are held for the first two weeks. Once 17 weeks have started, the first no
    // longer takes details (the 16 latest started periods do): it is paid by the request's
    // own destination, its proof naming no details, while the second is paid by its details.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed_to(&w, &mut shop, &mut wallet, weekly()).await;
    let all = weeks(17);
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(all[0], "lnbc1first", now + 300 * DAY_MS))
        .await
        .unwrap();
    w.clock.set_ms(all[1].starts_at);
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(all[1], "lnbc1second", now + 300 * DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.set_ms(all[16].starts_at);
    wallet.receive(None).await.unwrap();
    for (period, refs) in [(all[0], 1), (all[1], 2)] {
        by_user(&mut wallet, &request, period, PayOptions::default())
            .await
            .unwrap();
        let record = period_record(&w, &wallet, &request, period).await.unwrap();
        assert_eq!(proof_of(&record).0.message().refs.len(), refs, "{period:?}");
    }
}

#[tokio::test]
async fn test_period_payments_receipt_settles_its_periods_claim_by_evidence() {
    // January's payment ran but its outcome isn't written yet when the shop's receipt of it
    // comes: the receipt settles January's claim by evidence (Switch spec 10.2 row 3), on
    // the period's own obligation, in the batch that moves the cursor past it.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    let attempt = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap()
        .attempt_id();
    w.rail.settle(&attempt, true);
    let Some(RailOutcome::Paid(paid)) = w.rail.payment(attempt).await.unwrap() else {
        panic!("not paid");
    };
    let body = PaymentProof {
        attempt_id: attempt,
        billing_period: Some(january()),
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
    let proof = w.sign_as(
        &w.alice,
        "wallet",
        &w.bob,
        "paykit.payment_proof",
        vec![request.request_hash],
        body,
    );
    w.deliver(&w.alice, "wallet", &w.bob, &[shop.app_id()], &proof);
    shop.receive(None).await.unwrap();
    let period = shop.payee_activity().await.unwrap()[0].periods[0].record;
    shop.confirm_payment(&period).await.unwrap().unwrap();
    wallet.receive(None).await.unwrap();
    let obligation = period_obligation(&wallet, january()).await;
    let claim = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Settled, "{:?}", claim.view);
}

/// An hourly request for 1 btc from the start of the world's clock, paid to one invoice of
/// the sending app, its proposal open a day.
fn hourly() -> RequestDraft {
    let rule = AnchoredRule::new(START_MS, 1, Unit::Hour).unwrap();
    monthly().with_recurrence(Recurrence::new(rule, START_MS, None).unwrap())
}

/// The first Billing Period of [`hourly`].
fn first_hour() -> PeriodRef {
    let rule = AnchoredRule::new(START_MS, 1, Unit::Hour).unwrap();
    Recurrence::new(rule, START_MS, None)
        .unwrap()
        .periods(1)
        .unwrap()
        .into_iter()
        .map(PeriodRef::from)
        .next()
        .unwrap()
}

/// A store that runs a hook once, right after the commit that journals an attempt at a
/// stage: `"attempted"` once the switch applied its batch, `"paying"` just before the
/// payment method's call.
#[derive(Default)]
struct StageStore {
    inner: pubky_switch_client::store::MemoryStore,
    hook: std::sync::Mutex<Option<(&'static str, StageHook)>>,
}

/// A hook [`StageStore`] runs once.
type StageHook = Box<dyn FnOnce() + Send>;

impl StageStore {
    fn on_stage(&self, stage: &'static str, hook: impl FnOnce() + Send + 'static) {
        *self.hook.lock().unwrap() = Some((stage, Box::new(hook)));
    }
}

impl StateStore for StageStore {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_switch_client::store::KeyValue>, pubky_switch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        let armed = self.hook.lock().unwrap().as_ref().map(|(stage, _)| *stage);
        let due = armed.is_some_and(|stage| {
            let marker = format!("\"stage\":\"{stage}\"");
            batch.writes.iter().any(|write| {
                matches!(write, pubky_switch_client::store::Write::Put { key, value, .. }
                    if key.starts_with(b"p/j/")
                        && value.windows(marker.len()).any(|w| w == marker.as_bytes()))
            })
        });
        self.inner.commit(batch)?;
        let hook = if due {
            self.hook.lock().unwrap().take()
        } else {
            None
        };
        if let Some((_, hook)) = hook {
            hook();
        }
        Ok(())
    }
}

#[tokio::test]
async fn test_period_payments_by_policy_only_within_the_periods_automatic_window() {
    // January's automatic window ends when February starts: from then on January is paid
    // only by hand, the payment by policy writing nothing, and February by policy (Paykit
    // spec 8.2, "Paying periods").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    w.clock.set_ms(february().starts_at);
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NotAutomatic(why) if why.contains("window")),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(request_record(&w, &wallet, &request)
        .await
        .attempted_periods
        .is_empty());
    let outcome = by_policy(&mut wallet, &request, february()).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_by_policy_from_the_period_the_acceptance_was_recorded_in() {
    // The subscription started in January and is accepted in March: January and February
    // ended before the acceptance and are paid only by hand; March, which contains it, is
    // paid by policy (Paykit spec 8.2, "Paying periods").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let draft = monthly().with_proposal_expires_at_ms(START_MS + 90 * DAY_MS);
    let request = requested(&w, &mut shop, &mut wallet, draft).await;
    let march = periods(3)[2];
    w.clock.set_ms(march.starts_at + DAY_MS);
    wallet.accept_subscription(&request).await.unwrap();
    for period in [january(), february()] {
        let refused = by_policy(&mut wallet, &request, period).await.unwrap_err();
        assert!(
            matches!(refusal(&refused), Refusal::NotAutomatic(why) if why.contains("acceptance")),
            "{refused:?}"
        );
    }
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_policy(&mut wallet, &request, march).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let outcome = by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

/// The wallet, on a store that moves the clock 2 s on right after it journals `stage`, pays
/// the first hour of an hourly subscription by policy a second before the hour's automatic
/// window ends. Returns the world, the wallet, the request and the payment's result.
async fn dispatch_past_the_window(
    stage: &'static str,
) -> (World, Paykit, RequestRef, paykit_sdk::Result<PayOutcome>) {
    let w = World::new();
    let base = Arc::new(StageStore::default());
    let mut shop = w.shop().await;
    let mut wallet = w
        .wallet_on("wallet", Arc::clone(&base) as Arc<dyn StateStore>)
        .await;
    let (request, _) = subscribed_to(&w, &mut shop, &mut wallet, hourly()).await;
    w.clock.set_ms(first_hour().ends_at - 1_000);
    let clock = Arc::clone(&w.clock);
    base.on_stage(stage, move || clock.advance_ms(2_000));
    let outcome = by_policy(&mut wallet, &request, first_hour()).await;
    (w, wallet, request, outcome)
}

#[tokio::test]
async fn test_period_payments_by_policy_window_ended_before_the_dispatch_fails_the_attempt() {
    // The hour's automatic window ends after its attempt is journaled, once before the
    // `Paying` commit and once just after it: the attempt isn't dispatched, it is written
    // `failed`, and the user pays the hour by hand (Paykit spec 8.2).
    for stage in ["attempted", "paying"] {
        let (w, mut wallet, request, outcome) = Box::pin(dispatch_past_the_window(stage)).await;
        let refused = outcome.unwrap_err();
        assert!(
            matches!(refusal(&refused), Refusal::AttemptFailed(why)
                if matches!(*why, Refusal::NotAutomatic(_))),
            "{stage}: {refused:?}"
        );
        assert!(w.rail.all_executions().is_empty(), "{stage}");
        let outcome = by_user(&mut wallet, &request, first_hour(), PayOptions::default())
            .await
            .unwrap();
        assert!(
            matches!(outcome, PayOutcome::Paid { .. }),
            "{stage}: {outcome:?}"
        );
    }
}

#[tokio::test]
async fn test_period_payments_by_policy_repeat_completes_past_the_window() {
    // The hour's dispatch by policy is in flight when its automatic window ends: a resume's
    // repeat completes the payment already dispatched, the window judging only a first
    // dispatch (Paykit spec 8.2).
    let w = World::new();
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let mut shop = w.shop().await;
    let mut wallet = w
        .wallet_with("wallet", store(), crate::common::day_window())
        .await;
    let (request, _) = subscribed_to(&w, &mut shop, &mut wallet, hourly()).await;
    w.rail.next(Behaviour::Hang);
    let pending = by_policy(&mut wallet, &request, first_hour())
        .await
        .unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }), "{pending:?}");
    w.clock.set_ms(first_hour().ends_at + 60_000);
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&pending.attempt_id()), 2);
}

#[tokio::test]
async fn test_period_payments_request_marked_manual_only_pays_no_period_by_policy() {
    // The phone marks the subscription manual-only as a whole on its request record: no
    // period is paid by policy, at the wallet either, and the user pays each by hand
    // (Paykit spec 8.2, 8.3, "Manual only").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    phone.mark_manual_only(&request).await.unwrap();
    assert!(request_record(&w, &phone, &request).await.manual_only);
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.unwrap().manual_only);
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::ManualOnly);
    w.clock.set_ms(february().starts_at);
    let refused = by_policy(&mut wallet, &request, february())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::ManualOnly);
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_request_marked_after_the_paying_commit_holds_the_dispatch() {
    // Right after the wallet journals January's `Paying`, the phone marks the subscription
    // manual-only: the request record, read again from the switch before the dispatch,
    // holds it, and nothing is paid (Paykit spec 8.3, "Manual only").
    let w = World::new();
    let base = Arc::new(StageStore::default());
    let mut shop = w.shop().await;
    let mut wallet = w
        .wallet_on("wallet", Arc::clone(&base) as Arc<dyn StateStore>)
        .await;
    let mut phone = w.wallet("phone").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    base.on_stage("paying", move || {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async { phone.mark_manual_only(&request).await.unwrap() })
        })
        .join()
        .unwrap();
    });
    let outcome = Box::pin(by_policy(&mut wallet, &request, january()))
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert!(w.rail.all_executions().is_empty());
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
}

#[tokio::test]
async fn test_period_payments_period_marked_before_it_has_a_record_paid_only_by_hand() {
    // February has no period record yet: its mark creates one holding the mark and no
    // message, so no payment by policy is made of it, and the user's payment goes into it
    // (Paykit spec 8.3, "Manual only"; Paykit wire 5.3). A one-time request takes no period
    // mark.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    wallet
        .mark_period_manual_only(&request, &february())
        .await
        .unwrap();
    let record = period_record(&w, &wallet, &request, february())
        .await
        .unwrap();
    assert!(record.manual_only);
    assert!(record.stored().is_empty());
    w.clock.set_ms(february().starts_at);
    let refused = by_policy(&mut wallet, &request, february())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::ManualOnly);
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let record = period_record(&w, &wallet, &request, february())
        .await
        .unwrap();
    assert!(record.manual_only && record.proof.is_some());
    let one_time =
        crate::common::request_from(&mut shop, w.alice_id(), crate::common::draft("1", START_MS))
            .await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .mark_period_manual_only(&one_time, &january())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Usage(_)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_period_payments_period_mark_built_again_over_a_record_written_since() {
    // The phone took January's period details into a period record the wallet hasn't read:
    // the wallet's mark, built on no version, fails, and is built again on the record read
    // again, the details kept (Paykit spec 8.3, "Manual only").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1jan", now + DAY_MS))
        .await
        .unwrap();
    phone.receive(None).await.unwrap();
    assert!(period_record(&w, &wallet, &request, january())
        .await
        .is_none());
    wallet
        .mark_period_manual_only(&request, &january())
        .await
        .unwrap();
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert!(record.manual_only);
    assert_eq!(record.details().len(), 1);
}

#[tokio::test]
async fn test_period_payments_by_policy_fails_over_a_period_record_a_mark_created_since() {
    // The phone marks January before it has a period record; the wallet hasn't read it, and
    // its payment by policy, creating the record in its batch, fails: nothing is paid, and
    // read again, the mark refuses it (Paykit spec 8.3, "Manual only").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    phone.receive(None).await.unwrap();
    phone
        .mark_period_manual_only(&request, &january())
        .await
        .unwrap();
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert!(matches!(refused, PaykitError::Conflict(_)), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
    wallet.receive(None).await.unwrap();
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::ManualOnly);
}

/// How many `record.put`s alice's switch applied to the payer's period record of `period`,
/// as `app` keys it.
fn period_puts(w: &World, app: &Paykit, request: &RequestRef, period: PeriodRef) -> usize {
    let collection = Collection::PayerActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::period_record(&request.payee, &request.request_hash, period.start_secs()),
    );
    let key = app.app().record_key(&target).unwrap();
    w.alice
        .switch()
        .applied()
        .iter()
        .filter(|op| matches!(&op.body().kind, OpKind::RecordPut(put) if put.key == key))
        .count()
}

#[tokio::test]
async fn test_period_payments_by_policy_batch_writes_the_period_record() {
    // A payment by policy writes January's period record in its attempt's batch, creating
    // it, then its proof goes there; the user's payment of February writes only the proof
    // (Paykit spec 8.2, "Paying periods").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let outcome = by_policy(&mut wallet, &request, january()).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    w.clock.set_ms(february().starts_at);
    let outcome = by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(period_puts(&w, &wallet, &request, january()), 2);
    assert_eq!(period_puts(&w, &wallet, &request, february()), 1);
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert!(record.proof.is_some() && !record.manual_only);
}

/// The wallet on `base`, subscribed to bob's monthly request, holding January's period
/// details. Returns the shop, the wallet, the request and the key of January's period
/// record.
async fn holding_januarys_details(
    w: &World,
    base: Arc<dyn StateStore>,
) -> (Paykit, Paykit, RequestRef, [u8; 32]) {
    let mut shop = w.shop().await;
    let mut wallet = w.wallet_on("wallet", base).await;
    let (request, sale) = subscribed(w, &mut shop, &mut wallet).await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1jan", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let record = wallet.request(&request).await.unwrap().periods[0].record;
    (shop, wallet, request, record)
}

#[tokio::test]
async fn test_period_payments_divergent_period_record_pauses_only_its_period() {
    // January's period record is divergent: January is paid by policy never, and by hand
    // only once the user has seen it; February's period goes on (Paykit spec 6).
    let w = World::new();
    let base = store();
    let (_shop, mut wallet, request, record) =
        Box::pin(holding_januarys_details(&w, Arc::clone(&base))).await;
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Divergent { .. }),
        "{refused:?}"
    );
    let refused = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Divergent { records } if records[0].record == record),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_divergent_request_record_pauses_every_period() {
    // The request record is divergent: its upload may hold a cancellation or a mark, so
    // every period is paused, by policy and by hand, until the user has seen it; a period
    // past its deadline is refused for its deadline (Paykit spec 6).
    let w = World::new();
    let base = store();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let (request, _) = subscribed_to(
        &w,
        &mut shop,
        &mut wallet,
        monthly().with_period_deadline_s(DAY_MS / 1000),
    )
    .await;
    let record = wallet.request(&request).await.unwrap().record;
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    let refused = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Divergent { .. }),
        "{refused:?}"
    );
    let refused = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Divergent { .. }),
        "{refused:?}"
    );
    w.clock.advance_ms(2 * DAY_MS);
    wallet.receive(None).await.unwrap();
    let refused = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::DeadlinePassed);
    assert!(w.rail.all_executions().is_empty());
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    let outcome = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_period_record_divergent_after_the_paying_commit_holds_it() {
    // Right after the wallet journals January's `Paying` by policy, a view shows January's
    // period record divergent: the dispatch is held, nothing paid (Paykit spec 6, 8.3).
    let w = World::new();
    let base = Arc::new(StageStore::default());
    let (_shop, mut wallet, request, record) = Box::pin(holding_januarys_details(
        &w,
        Arc::clone(&base) as Arc<dyn StateStore>,
    ))
    .await;
    let store = Arc::clone(&base) as Arc<dyn StateStore>;
    let ns = pubky_switch_client::store::client_namespace(&wallet.app_id());
    base.on_stage("paying", move || {
        let key = format!("rcdiv/{}", pubky_switch_core::enc::b64(&record));
        let mut batch = pubky_switch_client::store::WriteBatch::new();
        batch
            .put(&ns, key.into_bytes(), br#"{"v":3,"d":null}"#.to_vec())
            .unwrap();
        store.commit(batch).unwrap();
    });
    let outcome = Box::pin(by_policy(&mut wallet, &request, january()))
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert!(w.rail.all_executions().is_empty());
}

/// Whether `app` keeps messages over `record` to write once its user has confirmed it
/// ([`paykit_sdk::DivergentRecord::writes_waiting`]).
async fn kept_over(app: &Paykit, record: &[u8; 32]) -> bool {
    app.divergent_records()
        .await
        .unwrap()
        .iter()
        .any(|shown| shown.record == *record && shown.writes_waiting)
}

#[tokio::test]
async fn test_period_payments_details_over_a_divergent_period_record_kept_until_confirmed() {
    // January's period record is divergent: later period details for it are kept, not
    // written, and the feed goes on past them, a later request taken; once the user
    // confirms the record, they are written (Paykit spec 6).
    let w = World::new();
    let base = store();
    let (mut shop, mut wallet, request, record) =
        Box::pin(holding_januarys_details(&w, Arc::clone(&base))).await;
    let sale = shop.payee_activity().await.unwrap()[0].record;
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    w.clock.advance_ms(1_000);
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1later", now + DAY_MS))
        .await
        .unwrap();
    let later = shop
        .send_request(w.alice_id(), crate::common::draft("2", now))
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    let held = wallet.request(&request).await.unwrap();
    assert_eq!(held.periods[0].details.len(), 1);
    assert!(wallet.request(&later).await.is_ok());
    // Kept over the record, they hold it after the confirmation until they are written:
    // January isn't paid by hand meanwhile.
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    assert!(kept_over(&wallet, &record).await);
    let refused = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Divergent { .. }),
        "{refused:?}"
    );
    wallet.receive(None).await.unwrap();
    assert!(!kept_over(&wallet, &record).await);
    let held = wallet.request(&request).await.unwrap();
    assert_eq!(held.periods[0].details.len(), 2);
}

#[tokio::test]
async fn test_period_payments_proof_over_a_divergent_period_record_kept_until_confirmed() {
    // The shop's period record of January is divergent: the wallet's proof of January is
    // kept, not written, and the feed goes on past it, a later rejection taken; once the
    // shop's user confirms the record, the proof is written (Paykit spec 6).
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
    let other = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("2", w.clock.now_ms()),
    )
    .await;
    let now = w.clock.now_ms();
    shop.send_period_details(&sale, details(january(), "lnbc1jan", now + DAY_MS))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let period = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .unwrap()
        .periods[0]
        .record;
    crate::divergent::set_divergent(&shop_store, &shop, &period, true);
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    wallet.reject_request(&other, None).await.unwrap();
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    let subscription = sales
        .iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .unwrap();
    assert!(!subscription.periods[0].paid);
    let rejected = sales
        .iter()
        .find(|a| a.request_hash == Some(other.request_hash))
        .unwrap();
    assert_eq!(rejected.status, PayeeStatus::Rejected);
    crate::divergent::confirm(&mut shop, &period).await.unwrap();
    assert!(kept_over(&shop, &period).await);
    shop.receive(None).await.unwrap();
    assert!(!kept_over(&shop, &period).await);
    let sales = shop.payee_activity().await.unwrap();
    let subscription = sales
        .iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .unwrap();
    assert!(subscription.periods[0].paid);
}
