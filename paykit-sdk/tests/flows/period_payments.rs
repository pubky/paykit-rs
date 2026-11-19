//! Paying a subscription's Billing Periods by hand (Paykit spec 7.2, 8.2, 8.3; Paykit wire
//! 4.10, 5.3): each period on its own obligation, to the request's destination, with
//! `attempted_periods`, the period's proof in its period record marked `manual_only`, the
//! payer's cancellation conditioned on the periods' claims, and the period's state read back
//! after a crash, a restore and a provider loss.

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_lib::bodies::{PaymentProof, PeriodDetails, RequestDestination};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
use paykit_lib::{Body, PaymentEndpointPayload, PaymentReference};
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitBackup, PaykitConfig, PaykitError,
    PeriodPrice, Rail, Refusal, RequestDraft, RequestRef, RequestStatus,
};
use pubky_switch_client::app::{
    Collection as SwitchCollection, OpBatch, RecordTarget, UserConfirmed,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{KeyValue, MemoryStore, StateStore, Write, WriteBatch};
use pubky_switch_client::testing::START_MS;
use pubky_switch_core::claims::{ClaimStatus, ClaimType};
use pubky_switch_core::ops::{Condition, OpKind};

use crate::common::{btc, endpoint, store, World, HOUR_MS};

const DAY_MS: u64 = 24 * HOUR_MS;

/// A monthly request for 1 btc from the start of the world's clock, paid to one invoice of
/// the sending app, its proposal open a day.
fn monthly() -> RequestDraft {
    let mut endpoints = BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt11"),
        PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
    );
    RequestDraft::new(
        btc("1"),
        PaymentReference::new("plan-1").unwrap(),
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(START_MS + DAY_MS)
    .with_recurrence(
        Recurrence::new(
            AnchoredRule::new(START_MS, 1, Unit::Month).unwrap(),
            START_MS,
            None,
        )
        .unwrap(),
    )
}

/// The request's first `n` Billing Periods, from the world's start.
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

fn fractional_monthly() -> Recurrence {
    let anchor = START_MS + 500;
    Recurrence::new(
        AnchoredRule::new(anchor, 1, Unit::Month).unwrap(),
        anchor,
        None,
    )
    .unwrap()
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
    Box::pin(app.pay_period(request, &period, options))
        .await
        .outcome
}

async fn by_policy(
    app: &mut Paykit,
    request: &RequestRef,
    period: PeriodRef,
) -> paykit_sdk::Result<PayOutcome> {
    Box::pin(app.pay_period_automatically(request, &period, PeriodPrice::RequestedAmount))
        .await
        .outcome
}

fn early() -> PayOptions {
    PayOptions::default().with_early_payment_confirmed(UserConfirmed::new())
}

/// The payer's record target at `natural`.
fn natural_target(natural: &[u8]) -> RecordTarget {
    let collection = Collection::PayerActivity;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        natural,
    )
}

fn period_natural(request: &RequestRef, period: PeriodRef) -> Vec<u8> {
    natural_key::period_record(&request.payee, &request.request_hash, period.start_secs())
}

/// The shared record key of `request`'s period record of `period`, as `app` keys it.
fn period_key(app: &Paykit, request: &RequestRef, period: PeriodRef) -> [u8; 32] {
    app.app()
        .record_key(&natural_target(&period_natural(request, period)))
        .unwrap()
}

/// The record `app` holds at the payer's `natural` key, read as alice's.
async fn payer_record(w: &World, app: &Paykit, natural: &[u8]) -> Option<ActivityRecord> {
    let key = app.app().record_key(&natural_target(natural)).unwrap();
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
    payer_record(w, app, &period_natural(request, period)).await
}

/// `app` writes `record` at the payer's `natural` key, built on the version it holds.
async fn write_payer_record(app: &mut Paykit, natural: &[u8], record: &ActivityRecord) {
    let op = app
        .app_mut()
        .record_op(
            &natural_target(natural),
            Some(&record.to_json().unwrap()),
            Vec::new(),
        )
        .await
        .unwrap();
    app.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
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

/// The `record.put`s alice's switch applied at the payer's `natural` key, as `app` keys it,
/// each with its conditions.
fn puts(w: &World, app: &Paykit, natural: &[u8]) -> Vec<Vec<Condition>> {
    let key = app.app().record_key(&natural_target(natural)).unwrap();
    w.alice
        .switch()
        .applied()
        .iter()
        .filter(|op| matches!(&op.body().kind, OpKind::RecordPut(put) if put.key == key))
        .map(|op| op.body().conditions.clone())
        .collect()
}

fn request_puts(w: &World, app: &Paykit, request: &RequestRef) -> Vec<Vec<Condition>> {
    puts(
        w,
        app,
        &natural_key::request_record(&request.payee, &request.request_hash),
    )
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

async fn restore_before_first_read(w: &World, backup: &PaykitBackup) -> Paykit {
    Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

async fn restored(w: &World, backup: &PaykitBackup) -> Paykit {
    let mut app = restore_before_first_read(w, backup).await;
    app.receive(None).await.unwrap();
    app
}

#[tokio::test]
async fn test_period_payments_pays_a_period_by_hand_to_the_requests_destination() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // The request record lists the period; the period record holds the proof, which names
    // the period and only the request, and the mark of a payment by hand (Paykit wire 4.10,
    // 5.3; Paykit spec 8.3, "Manual only").
    let listed = request_record(&w, &wallet, &request).await;
    assert_eq!(listed.attempted_periods, [january().start_secs()]);
    assert!(listed.proof.is_none() && !listed.manual_only);
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert!(record.manual_only);
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
        .outcome
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Recurring);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_period_payments_credit_a_late_proof_settled_at_the_deadline() {
    Box::pin(confirm_period_settlement(
        SettlementTerms::Fixed(Some(60)),
        60_000,
        true,
    ))
    .await;
}

#[tokio::test]
async fn test_period_payments_keep_a_payment_settled_after_the_deadline_uncredited() {
    Box::pin(confirm_period_settlement(
        SettlementTerms::Fixed(Some(60)),
        60_001,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_period_payments_credit_a_later_settlement_without_a_deadline() {
    Box::pin(confirm_period_settlement(
        SettlementTerms::Fixed(None),
        60_001,
        true,
    ))
    .await;
}

#[tokio::test]
async fn test_period_payments_priced_settlement_observes_the_rate_and_period_deadline() {
    // The signed rate expires 120 seconds after the period starts.
    for (deadline_s, settled_after_ms, credited) in [
        (60, 60_000, true),
        (60, 60_001, false),
        (180, 120_000, true),
        (180, 120_001, false),
    ] {
        Box::pin(confirm_period_settlement(
            SettlementTerms::SignedRate(deadline_s),
            settled_after_ms,
            credited,
        ))
        .await;
    }
}

enum SettlementTerms {
    Fixed(Option<u64>),
    SignedRate(u64),
}

async fn confirm_period_settlement(terms: SettlementTerms, after_start_ms: u64, credited: bool) {
    let (w, mut shop, mut payer, request, options) = match terms {
        SettlementTerms::Fixed(deadline_s) => {
            let w = World::new();
            let mut shop = w.shop().await;
            let mut payer = w.wallet("wallet").await;
            let mut draft = monthly();
            if let Some(seconds) = deadline_s {
                draft = draft.with_period_deadline_s(seconds);
            }
            let request = requested(&w, &mut shop, &mut payer, draft).await;
            payer.accept_subscription(&request).await.unwrap();
            shop.receive(None).await.unwrap();
            (w, shop, payer, request, PayOptions::default())
        }
        SettlementTerms::SignedRate(seconds) => {
            let (w, shop, mut payer, request, msg_id) =
                Box::pin(priced_period_on(false, store(), Some(seconds))).await;
            let shown = crate::conversion::priced_of(
                &by_user(&mut payer, &request, january(), PayOptions::default())
                    .await
                    .unwrap_err(),
            );
            assert_eq!(
                shown.source,
                paykit_sdk::RateSource::PeriodDetails { msg_id }
            );
            let options = PayOptions::default().with_rate_authorized(shown);
            (w, shop, payer, request, options)
        }
    };

    w.rail.next(Behaviour::Hang);
    let pending = by_user(&mut payer, &request, january(), options)
        .await
        .unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }));
    let attempt = pending.attempt_id();
    let paid_at = january().starts_at + after_start_ms;
    w.clock.advance_ms(paid_at - w.clock.now_ms());
    w.rail.settle(&attempt, true);
    // The payer first learns the result after the deadline, even when it was paid on time.
    w.clock.advance_ms(1_000);
    let outcome = payer.resume_payments().await.unwrap();
    assert!(matches!(outcome.as_slice(), [PayOutcome::Paid { .. }]));

    let mut second = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut issued = None;
    for till in [&mut shop, &mut second] {
        till.receive(None).await.unwrap();
        let activity = till.payee_activity().await.unwrap();
        let period = &activity[0].periods[0];
        assert_eq!(period.billing_period, january());
        assert!(period.paid);
        assert_eq!(
            till.verify_payment(&period.record).await.unwrap(),
            credited.then_some(paid_at)
        );
        let receipt = till.confirm_payment(&period.record).await.unwrap();
        assert_eq!(receipt.is_some(), credited);
        if let Some(previous) = &issued {
            assert_eq!(receipt.as_ref(), Some(previous));
        }
        issued = receipt;
        assert_eq!(till.confirm_payment(&period.record).await.unwrap(), issued);
        let activity = till.payee_activity().await.unwrap();
        assert_eq!(activity[0].periods[0].credited, credited);
        assert_eq!(
            activity[0].periods[0].receipt.as_ref().map(|r| &r.msg_id),
            issued.as_ref()
        );
    }
    payer.receive(None).await.unwrap();
    let receipts = payer
        .receipts(paykit_sdk::ReceiptsOf::Period(request, january()))
        .await
        .unwrap();
    assert_eq!(
        receipts.iter().map(|r| &r.msg_id).collect::<Vec<_>>(),
        issued.iter().collect::<Vec<_>>()
    );
    assert_eq!(w.rail.pay_calls(&attempt), 1);
}

#[tokio::test]
async fn test_period_payments_refused_for_what_stops_the_period() {
    // A rejected subscription pays no period; a period past its payment deadline isn't
    // paid; a blocked payee's period is paid only once the user confirms that exact payment,
    // its period included (Paykit spec 7.1, 8.1, 8.2).
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
            PaykitConfig::default(),
            store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let rejected = requested(&w, &mut shop, &mut wallet, monthly()).await;
    wallet.reject_request(&rejected, None).await.unwrap();
    let refused = by_user(&mut wallet, &rejected, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Rejected);
    let (request, _) = subscribed_to(
        &w,
        &mut shop,
        &mut wallet,
        monthly().with_period_deadline_s(DAY_MS / 1000),
    )
    .await;
    w.clock.advance_ms(2 * DAY_MS);
    wallet.receive(None).await.unwrap();
    let refused = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::DeadlinePassed);
    assert!(w.rail.all_executions().is_empty());
    wallet.block(&w.bob_id()).await.unwrap();
    let refused = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap_err();
    let Some(Refusal::Blocked { payment }) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert_eq!(payment.period, Some(february()));
    assert!(w.rail.all_executions().is_empty());
    let confirmed = early().with_blocked_confirmed((**payment).clone());
    let outcome = by_user(&mut wallet, &request, february(), confirmed)
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_later_period_with_per_period_endpoints_has_no_destination_here() {
    // With `per_period_endpoints`, the request's endpoints serve only its first period: a
    // later one is paid to its period details' destination, never to the request's (Paykit
    // spec 8.2, "Destinations per period").
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed_to(
        &w,
        &mut shop,
        &mut wallet,
        monthly().with_per_period_endpoints(),
    )
    .await;
    w.clock.set_ms(february().starts_at);
    wallet.receive(None).await.unwrap();
    let refused = by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NoDestination(_)),
        "{refused:?}"
    );
    assert!(period_record(&w, &wallet, &request, february())
        .await
        .is_none());
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
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
    assert!(w.rail.all_executions().is_empty());
    assert!(period_record(&w, &wallet, &request, february())
        .await
        .is_none());
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
    // January finished: the write that adds February drops it, conditioned on January's
    // claim at the step read (Paykit spec 8.2, "Attempted periods").
    let record = request_record(&w, &wallet, &request).await;
    assert_eq!(record.attempted_periods, [all[1].start_secs()]);
    let january_claim = period_obligation(&wallet, all[0]).await;
    assert!(request_puts(&w, &wallet, &request)
        .last()
        .unwrap()
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
    let before = request_puts(&w, &wallet, &request).len();
    w.rail.settle(&pending.attempt_id(), false);
    wallet.resume_payments().await.unwrap();
    by_user(&mut wallet, &request, all[1], early())
        .await
        .unwrap();
    assert_eq!(request_puts(&w, &wallet, &request).len(), before);
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
    // A paid period doesn't hold the cancellation back; it is conditioned on the listed
    // period's claim (Paykit spec 8.2, "Ending").
    wallet.cancel_request(&request, None).await.unwrap();
    let january_claim = period_obligation(&wallet, january()).await;
    assert!(request_puts(&w, &wallet, &request)
        .last()
        .unwrap()
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
async fn test_period_payments_cancellation_refused_while_a_listed_period_may_be_paid() {
    // The phone's attempt on January is open: the wallet's cancellation reads the listed
    // period's claim and is refused as in progress. Turned `unknown`, the period stays
    // listed, also through a write that adds another, and the cancellation is refused as
    // may be paid. Once its owner reports it failed, the cancellation goes (Paykit spec 8.2,
    // "Attempted periods", "Ending").
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
    wallet.receive(None).await.unwrap();
    let refused = wallet.cancel_request(&request, None).await.unwrap_err();
    assert_eq!(refusal(&refused), Refusal::PaymentInProgress);
    w.clock.advance_ms(4201 * 1000);
    wallet.receive(None).await.unwrap();
    wallet
        .resolve_attempt(
            &obligation,
            attempt,
            paykit_sdk::Resolution::Unknown,
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    let refused = wallet.cancel_request(&request, None).await.unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::MayBePaid { .. }),
        "{refused:?}"
    );
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
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Cancelled
    );
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
    w.clock.set_ms(february().starts_at);
    let refused = by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&refused), Refusal::Cancelled);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_period_payments_hand_mark_creates_or_keeps_the_period_record() {
    // January has no period record: the user's payment creates it in its attempt's batch,
    // holding the mark and no message, built on no version. February's record holds period
    // details: the batch writes it at the version read with `manual_only` set and nothing
    // else changed (Paykit spec 8.3, "Manual only"; Paykit wire 5.3).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    let pending = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }), "{pending:?}");
    assert_eq!(
        puts(&w, &wallet, &period_natural(&request, january())).len(),
        1
    );
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert!(record.manual_only && record.stored().is_empty());
    assert_eq!(record.billing_period, Some(january()));
    assert!(!request_record(&w, &wallet, &request).await.manual_only);
    w.clock.set_ms(february().starts_at);
    shop.send_period_details(
        &sale,
        details(february(), "lnbc1feb", february().starts_at + DAY_MS),
    )
    .await
    .unwrap();
    wallet.receive(None).await.unwrap();
    let before = period_record(&w, &wallet, &request, february())
        .await
        .unwrap();
    assert_eq!(before.details().len(), 1);
    w.rail.next(Behaviour::Hang);
    by_user(&mut wallet, &request, february(), PayOptions::default())
        .await
        .unwrap();
    let mut expected = before.clone();
    expected.manual_only = true;
    let after = period_record(&w, &wallet, &request, february())
        .await
        .unwrap();
    assert_eq!(after.to_json().unwrap(), expected.to_json().unwrap());
    // The request's destination was paid, not the optional details' (Paykit spec 8.2).
    let entries = wallet.app().protocol_scan("paykit", b"j/").await.unwrap();
    assert!(entries.iter().all(|(_, bytes)| {
        let entry: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        entry["d"]["destination"]["payload"] == "lnbc1invoice"
    }));
}

#[tokio::test]
async fn test_period_payments_hand_batch_in_conflict_writes_no_attempt_and_no_mark() {
    // The phone writes January's period record after the wallet read it. The wallet's payment
    // by hand marks the record at the version it read: the switch refuses the batch, and
    // neither the attempt nor the mark is written. The record is read again on that
    // conflict, and the next try lands the attempt and the mark together (Paykit spec 8.3).
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
    let record = period_record(&w, &phone, &request, january())
        .await
        .unwrap();
    let natural = period_natural(&request, january());
    write_payer_record(&mut phone, &natural, &record).await;
    let before = puts(&w, &phone, &natural).len();
    let conflict = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(conflict, PaykitError::Conflict(_)), "{conflict:?}");
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(puts(&w, &phone, &natural).len(), before);
    assert!(request_record(&w, &wallet, &request)
        .await
        .attempted_periods
        .is_empty());
    let paid = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert!(record.manual_only && record.proof.is_some());
}

#[tokio::test]
async fn test_period_payments_hand_batch_kept_unanswered_is_no_payment() {
    // The attempt's batch, carrying the mark, gets no answer: the client keeps it, and the
    // call returns its error with the batch's ID, nothing paid; the mark lands only with the
    // attempt, once the client sends the batch again and it applies (Paykit spec 8.3).
    use pubky_switch_client::testing::Fault;
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    let error = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(error.kept_batch().is_some(), "{error:?}");
    assert!(w.rail.all_executions().is_empty());
    assert!(period_record(&w, &wallet, &request, january())
        .await
        .is_none());
    Box::pin(wallet.flush()).await.unwrap();
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert!(record.manual_only);
    assert_eq!(
        request_record(&w, &wallet, &request)
            .await
            .attempted_periods,
        [january().start_secs()]
    );
}

#[tokio::test]
async fn test_period_payments_done_by_hand_requires_the_attempts_proof() {
    check_period_resolution(false).await;
}

#[tokio::test]
async fn test_period_payments_signed_details_do_not_establish_a_paid_attempt() {
    check_period_resolution(true).await;
}

async fn check_period_resolution(with_details: bool) {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let phone_store = store();
    let mut phone = w.wallet_on("phone", Arc::clone(&phone_store)).await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    if with_details {
        shop.send_period_details(
            &sale,
            crate::periods::january_details(w.clock.now_ms() + DAY_MS),
        )
        .await
        .unwrap();
        wallet.receive(None).await.unwrap();
    }
    w.rail.next(Behaviour::Hang);
    let attempt = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap()
        .attempt_id();
    let obligation = period_obligation(&wallet, january()).await;
    let natural = period_natural(&request, january());
    w.clock.advance_ms(4201 * 1000);
    phone.receive(None).await.unwrap();
    let phone_id = phone.app_id();
    drop(phone);
    let mut phone = w
        .load_on_clock(&w.alice, phone_store, phone_id, Arc::clone(&w.clock))
        .await;
    let before = period_record(&w, &phone, &request, january())
        .await
        .unwrap();
    assert!(before.manual_only && !before.shows_paid());
    assert_eq!(before.stored().is_empty(), !with_details);
    let writes = puts(&w, &phone, &natural).len();
    let old_step = phone
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap()
        .view
        .step;
    for resolver in [&mut wallet, &mut phone] {
        let error = resolver
            .resolve_attempt(
                &obligation,
                attempt,
                paykit_sdk::Resolution::Done,
                UserConfirmed::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error.refusal(), Some(Refusal::RecordHeld(_))),
            "{error:?}"
        );
        let claim = resolver
            .app_mut()
            .read_claim(&obligation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.view.step, old_step);
        assert_eq!(claim.view.status, ClaimStatus::Attempting);
    }
    assert_eq!(puts(&w, &phone, &natural).len(), writes);
    assert_eq!(
        period_record(&w, &phone, &request, january())
            .await
            .unwrap()
            .to_json()
            .unwrap(),
        before.to_json().unwrap()
    );
    assert_eq!(w.rail.all_executions().values().sum::<u64>(), 1);

    // The original method subsequently confirms that very attempt. Its completion
    // leaves signed evidence available to another wallet after the no-deadline claim expires.
    w.rail.settle(&attempt, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    let paid = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert_eq!(proof_of(&paid).1.attempt_id, attempt);
    w.clock.advance_ms(401 * DAY_MS);
    let mut later = w.wallet("later-wallet").await;
    later.receive(None).await.unwrap();
    assert!(later
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        proof_of(
            &period_record(&w, &later, &request, january())
                .await
                .unwrap()
        )
        .1
        .attempt_id,
        attempt
    );
    let refused = by_user(&mut later, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::AlreadyPaid)),
        "{refused:?}"
    );
    assert_eq!(w.rail.all_executions().values().sum::<u64>(), 1);
}

#[tokio::test]
async fn test_period_payments_restore_an_orphan_then_confirm_the_owners_paid_proof() {
    use pubky_switch_client::store::{Fault, FaultStore};
    let w = World::new();
    let mut shop = w.shop().await;
    let fault = Arc::new(FaultStore::new(store(), Fault::None));
    let mut wallet = w.wallet_on("wallet", fault.clone()).await;
    let mut phone = w.wallet("phone").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    let attempt = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap()
        .attempt_id();
    let obligation = period_obligation(&wallet, january()).await;
    phone.receive(None).await.unwrap();
    let pending = phone.backup().await.unwrap();
    drop(phone);
    w.clock.advance_ms(4201 * 1000);
    let mut phone = restored(&w, &pending).await;
    phone.reconcile().await.unwrap();
    let error = phone
        .resolve_attempt(
            &obligation,
            attempt,
            paykit_sdk::Resolution::Done,
            UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal(), Some(Refusal::RecordHeld(_))),
        "{error:?}"
    );
    let open = phone
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(open.view.status, ClaimStatus::Attempting);
    let mark = period_record(&w, &phone, &request, january())
        .await
        .unwrap();
    assert!(mark.manual_only && mark.stored().is_empty());

    // The method confirms payment. Briefly unavailable local storage prevents
    // the first outcome batch from being prepared, leaving its proof in the journal.
    w.rail.settle(&attempt, true);
    fault.fail_put_once(b"opq/");
    let interrupted = wallet.resume_payments().await;
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid,
        "{interrupted:?}"
    );
    let open = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(open.view.status, ClaimStatus::Attempting);
    wallet
        .resolve_attempt(
            &obligation,
            attempt,
            paykit_sdk::Resolution::Done,
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        wallet
            .app_mut()
            .read_claim(&obligation)
            .await
            .unwrap()
            .unwrap()
            .view
            .status,
        ClaimStatus::Settled
    );
    phone.receive(None).await.unwrap();
    let record = period_record(&w, &phone, &request, january())
        .await
        .unwrap();
    let (_, proof) = proof_of(&record);
    assert_eq!(proof.attempt_id, attempt);
    assert_eq!(proof.billing_period, Some(january()));
    assert_eq!(w.rail.executions(&attempt), 1);
}

/// A backup taken after the rail paid can predate the original wallet's
/// successful outcome batch. Its signed proof remains the same payment.
async fn restore_before_original_completion(answered: bool, priced: bool) {
    use pubky_switch_client::store::{Fault, FaultStore};
    let fault = Arc::new(FaultStore::new(store(), Fault::None));
    let (w, mut shop, mut wallet, request, rate) = if priced {
        let (w, shop, wallet, request, id) =
            Box::pin(priced_period_on(false, fault.clone(), None)).await;
        (w, shop, wallet, request, Some(id))
    } else {
        let w = World::new();
        let mut shop = w.shop().await;
        let mut wallet = w.wallet_on("wallet", fault.clone()).await;
        let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
        (w, shop, wallet, request, None)
    };
    let period = january();
    let natural = period_natural(&request, period);
    let mut options = PayOptions::default();
    if rate.is_some() {
        let shown = by_user(&mut wallet, &request, period, PayOptions::default())
            .await
            .unwrap_err();
        options = options.with_rate_authorized(crate::conversion::priced_of(&shown));
    }
    w.rail.next(Behaviour::Hang);
    let attempt = by_user(&mut wallet, &request, period, options)
        .await
        .unwrap()
        .attempt_id();
    let obligation = period_obligation(&wallet, period).await;
    w.rail.settle(&attempt, true);
    fault.fail_put_once(b"opq/");
    let interrupted = wallet.resume_payments().await;
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid,
        "{interrupted:?}"
    );
    let backup = wallet.backup().await.unwrap();
    // A second installation can finish its first restore read before the original
    // payer publishes its outcome. Its own outbox has never held that proof.
    let early = if answered {
        None
    } else {
        Some(restored(&w, &backup).await)
    };

    let first = wallet.resume_payments().await.unwrap();
    let [PayOutcome::Paid {
        attempt_id,
        proof_msg_id: Some(original_id),
        ..
    }] = first.as_slice()
    else {
        panic!("original completion: {first:?}");
    };
    assert_eq!(attempt_id, &attempt);
    let first_record = period_record(&w, &wallet, &request, period).await.unwrap();
    let (signed, body) = proof_of(&first_record);
    assert_eq!(&signed.message().msg_id, original_id);
    assert_eq!(body.attempt_id, attempt);
    assert_eq!(body.billing_period, Some(period));
    let original_hash = signed.hash();
    if let Some(rate) = rate {
        let details = first_record.period_details.as_ref().unwrap();
        assert_eq!(details.message().msg_id, rate);
        assert_eq!(
            signed.message().refs,
            [request.request_hash, details.hash()]
        );
        assert_eq!(w.rail.paid_amount(&attempt), Some(btc("0.00012346")));
    }
    assert!(matches!(
        wallet.app().delivery(original_id).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
    ));
    shop.receive(None).await.unwrap();
    if answered {
        let sale = shop.payee_activity().await.unwrap();
        let id = Box::pin(shop.confirm_payment(&sale[0].periods[0].record))
            .await
            .unwrap()
            .unwrap();
        wallet.receive(None).await.unwrap();
        let paid = period_record(&w, &wallet, &request, period).await.unwrap();
        let receipt = paid.receipt.unwrap();
        assert_eq!(receipt.message().msg_id, id);
        assert_eq!(
            receipt.message().refs,
            vec![original_hash, request.request_hash]
        );
    }
    let paid = period_record(&w, &wallet, &request, period)
        .await
        .unwrap()
        .to_json()
        .unwrap();
    let writes = puts(&w, &wallet, &natural).len();
    let claim_entries = w
        .alice
        .switch()
        .applied()
        .iter()
        .filter(
            |op| matches!(&op.body().kind, OpKind::Claim(c) if c.claim_key == obligation.to_vec()),
        )
        .count();
    drop(wallet);

    let mut wallet = match early {
        Some(mut early) => {
            early.receive(None).await.unwrap();
            early
        }
        None => restored(&w, &backup).await,
    };
    assert!(wallet.app().delivery(original_id).await.unwrap().is_none());
    let report = wallet.reconcile().await.unwrap();
    assert_eq!(report.resolved, first);
    assert!(wallet.app().delivery(original_id).await.unwrap().is_none());
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(w.rail.executions(&attempt), 1);
    let current = period_record(&w, &wallet, &request, period).await.unwrap();
    assert_eq!(current.to_json().unwrap(), paid);
    assert_eq!(puts(&w, &wallet, &natural).len(), writes);
    assert_eq!(w.alice.switch().applied().iter()
        .filter(|op| matches!(&op.body().kind, OpKind::Claim(c) if c.claim_key == obligation.to_vec())).count(), claim_entries);
    assert_eq!(
        wallet
            .app_mut()
            .read_claim(&obligation)
            .await
            .unwrap()
            .unwrap()
            .view
            .status,
        ClaimStatus::Settled
    );
}

#[tokio::test]
async fn test_period_payments_retained_delete_holds_the_period_after_claim_retention() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut first = w.wallet("payer").await;
    let (request, _) = subscribed(&w, &mut shop, &mut first).await;
    let period = january();
    let attempt = by_user(&mut first, &request, period, PayOptions::default())
        .await
        .unwrap()
        .attempt_id();
    let obligation = period_obligation(&first, period).await;
    let issued = period_record(&w, &first, &request, period).await.unwrap();
    assert_eq!(proof_of(&issued).1.attempt_id, attempt);
    let mut other = w.wallet("other-payer").await;
    let natural = period_natural(&request, period);
    let deleted = other
        .app_mut()
        .delete_record(&natural_target(&natural), Vec::new())
        .await
        .unwrap();
    assert_eq!(
        deleted.content,
        pubky_switch_client::app::RecordContent::Deleted
    );
    w.clock.advance_ms(401 * DAY_MS);
    let base = store();
    let mut wallet = w.wallet_on("later-payer", Arc::clone(&base)).await;
    let id = wallet.app_id();
    assert!(wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    let key = period_key(&wallet, &request, period);
    let mut backup = None;
    for visit in 0..3 {
        wallet.receive(None).await.unwrap();
        let before = puts(&w, &wallet, &natural).len();
        let attempts_before = attempt_conditions(&w, &obligation).len();
        let error = by_user(&mut wallet, &request, period, PayOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(error.refusal(), Some(Refusal::RecordHeld(_))),
            "visit {visit}: {error:?}"
        );
        let held = wallet
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .expect("pay_period checked the retained deletion");
        assert_eq!(held.version, deleted.version, "visit {visit}");
        assert_eq!(held.content, deleted.content);
        assert_eq!(puts(&w, &wallet, &natural).len(), before);
        assert_eq!(attempt_conditions(&w, &obligation).len(), attempts_before);
        assert_eq!(w.rail.all_executions().values().sum::<u64>(), 1);
        if visit == 0 {
            backup = Some(wallet.backup().await.unwrap());
            drop(wallet);
            wallet = w
                .load_on_clock(&w.alice, Arc::clone(&base), id, Arc::clone(&w.clock))
                .await;
        } else if visit == 1 {
            drop(wallet);
            wallet = restored(&w, backup.as_ref().unwrap()).await;
            wallet.reconcile().await.unwrap();
            wallet.end_reconcile(UserConfirmed::new()).await.unwrap();
        }
    }
}

#[tokio::test]
async fn test_period_payments_restore_the_original_paid_proof_with_its_receipt() {
    Box::pin(restore_before_original_completion(true, false)).await;
}

#[tokio::test]
async fn test_period_payments_restore_the_original_unanswered_paid_proof() {
    Box::pin(restore_before_original_completion(false, false)).await;
}

#[tokio::test]
async fn test_period_payments_restore_the_original_priced_proof_with_its_receipt() {
    Box::pin(restore_before_original_completion(true, true)).await;
}

#[tokio::test]
async fn test_period_payments_restore_the_original_unanswered_priced_proof() {
    Box::pin(restore_before_original_completion(false, true)).await;
}

#[tokio::test]
async fn test_period_payments_divergent_records_pause_their_periods_until_confirmed() {
    // January's period record is divergent: January is paid by hand only once the user has
    // seen it, February goes on. Then the request record is divergent: every period waits
    // (Paykit spec 6).
    let w = World::new();
    let base = store();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let (request, sale) = subscribed(&w, &mut shop, &mut wallet).await;
    shop.send_period_details(
        &sale,
        details(january(), "lnbc1jan", w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    wallet.receive(None).await.unwrap();
    let january_record = period_key(&wallet, &request, january());
    crate::divergent::set_divergent(&base, &wallet, &january_record, true);
    let refused = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Divergent { records } if records[0].record == january_record),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    crate::divergent::confirm(&mut wallet, &january_record)
        .await
        .unwrap();
    let request_key = wallet.request(&request).await.unwrap().record;
    crate::divergent::set_divergent(&base, &wallet, &request_key, true);
    let refused = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::Divergent { records } if records[0].record == request_key),
        "{refused:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    crate::divergent::confirm(&mut wallet, &request_key)
        .await
        .unwrap();
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

async fn priced_period_on(
    with_endpoints: bool,
    base: Arc<dyn StateStore>,
    period_deadline_s: Option<u64>,
) -> (World, Paykit, Paykit, RequestRef, String) {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet_on("wallet", base).await;
    let rule = AnchoredRule::new(START_MS, 1, Unit::Month).unwrap();
    let mut draft = crate::conversion::priced(
        START_MS,
        &[("btc-lightning-bolt11", "lnbc1invoice")],
        &[("btc", "0.0000123456")],
    )
    .with_conversion(paykit_lib::bodies::Conversion::PerPeriod)
    .with_recurrence(Recurrence::new(rule, START_MS, None).unwrap());
    if let Some(seconds) = period_deadline_s {
        draft = draft.with_period_deadline_s(seconds);
    }
    let (request, sale) = subscribed_to(&w, &mut shop, &mut wallet, draft).await;
    let period = january();
    w.clock.set_ms(period.starts_at);
    let mut rate_details = details(period, "lnbc1period", period.starts_at + 120_000);
    if !with_endpoints {
        rate_details.payment_endpoints = None;
    }
    rate_details.rates = Some(vec![paykit_lib::Rate::new(
        paykit_lib::Asset::new("btc").unwrap(),
        paykit_lib::Decimal::new("0.0000123456").unwrap(),
    )
    .unwrap()]);
    let msg_id = shop
        .send_period_details(&sale, rate_details)
        .await
        .unwrap()
        .msg_id;
    wallet.receive(None).await.unwrap();
    (w, shop, wallet, request, msg_id)
}

#[tokio::test]
async fn test_period_payments_standing_rate_policy_checks_each_occurrence_and_keeps_a_started_attempt(
) {
    let w = World::new();
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let base = store();
    let mut wallet = w.wallet_on("wallet", base.clone()).await;
    let mut shop = w.shop().await;
    let recurrence = Recurrence::new(
        AnchoredRule::new(START_MS, 1, Unit::Hour).unwrap(),
        START_MS,
        None,
    )
    .unwrap();
    let periods = recurrence
        .periods(4)
        .unwrap()
        .into_iter()
        .map(PeriodRef::from)
        .collect::<Vec<_>>();
    let draft = crate::conversion::priced(
        START_MS,
        &[("btc-lightning-bolt11", "lnbc1invoice")],
        &[("btc", "0.0000123456")],
    )
    .with_recurrence(recurrence);
    let (request, _) = subscribed_to(&w, &mut shop, &mut wallet, draft).await;
    let ceiling = crate::common::btc("0.0002");
    let shop_app = shop.app_id();
    let authorize = |quote: &paykit_sdk::PricedPayment, period| {
        quote.request == request
            && quote.billing_period == Some(period)
            && quote.app_id == shop_app
            && quote.endpoint_id.as_str() == "btc-lightning-bolt11"
            && quote.amount.asset == ceiling.asset
            && quote.amount.value <= ceiling.value
    };
    let endpoint = crate::common::endpoint("btc-lightning-bolt11");
    let first = crate::conversion::priced_of(
        &wallet
            .pay_period_automatically(
                &request,
                &periods[0],
                PeriodPrice::QuoteFor(endpoint.clone()),
            )
            .await
            .outcome
            .unwrap_err(),
    );
    assert_eq!(first.rate_source_hash, request.request_hash);
    assert!(authorize(&first, periods[0]));
    let first_payment = wallet
        .pay_period_automatically(&request, &periods[0], PeriodPrice::AtRate(&first))
        .await
        .outcome
        .unwrap();
    assert!(matches!(first_payment, PayOutcome::Paid { .. }));
    w.clock.set_ms(periods[1].starts_at);
    let prior = wallet
        .pay_period_automatically(&request, &periods[1], PeriodPrice::AtRate(&first))
        .await
        .outcome
        .unwrap_err();
    let second = crate::conversion::priced_of(&prior);
    assert_eq!(second.billing_period, Some(periods[1]));
    let mut changed = second.clone();
    changed.rate_source_hash[0] ^= 1;
    let refused = wallet
        .pay_period_automatically(&request, &periods[1], PeriodPrice::AtRate(&changed))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(crate::conversion::priced_of(&refused), second);
    assert_eq!(wallet.payments().await.unwrap().len(), 1);
    w.rail.next(Behaviour::Hang);
    assert!(authorize(&second, periods[1]));
    let pending = wallet
        .pay_period_automatically(&request, &periods[1], PeriodPrice::AtRate(&second))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = pending else {
        panic!("{pending:?}")
    };
    let wallet_id = wallet.app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, base, wallet_id, Arc::clone(&w.clock))
        .await;
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.paid_amount(&attempt_id), Some(second.amount));
    assert_eq!(w.rail.executions(&attempt_id), 1);
    w.clock.set_ms(periods[2].starts_at);
    let missed = crate::conversion::priced_of(
        &wallet
            .pay_period_automatically(&request, &periods[2], PeriodPrice::QuoteFor(endpoint))
            .await
            .outcome
            .unwrap_err(),
    );
    w.clock.set_ms(periods[3].starts_at);
    assert!(authorize(&missed, periods[2]));
    let refused = wallet
        .pay_period_automatically(&request, &periods[2], PeriodPrice::AtRate(&missed))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::NotAutomatic(_)),
        "{refused:?}"
    );
    assert_eq!(wallet.payments().await.unwrap().len(), 2);
    assert_eq!(w.rail.all_executions().len(), 2);
}

#[tokio::test]
async fn test_period_payments_quote_for_leaves_restore_and_list_requests_to_the_host() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut draft = monthly();
    draft.destination = RequestDestination::Any;
    let (request, _) = subscribed_to(&w, &mut shop, &mut wallet, draft).await;
    let endpoint = crate::common::endpoint("btc-lightning-bolt11");
    let sent = wallet
        .app()
        .sent_to(&request.payee, 0, u64::MAX)
        .await
        .unwrap()
        .len();
    let quoted = wallet
        .pay_period_automatically(
            &request,
            &january(),
            PeriodPrice::QuoteFor(endpoint.clone()),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refusal(&quoted), Refusal::NotNow(why) if why.contains("Private Payment List")),
        "{quoted:?}"
    );
    assert_eq!(
        wallet
            .app()
            .sent_to(&request.payee, 0, u64::MAX)
            .await
            .unwrap()
            .len(),
        sent
    );
    assert!(wallet.payments().await.unwrap().is_empty());
    let backup = wallet.backup().await.unwrap();
    let mut imported = restore_before_first_read(&w, &backup).await;
    assert!(imported.app().restore_held_cursor().is_none());
    let quote = imported
        .pay_period_automatically(&request, &january(), PeriodPrice::QuoteFor(endpoint))
        .await;
    assert!(quote.outcome.is_err());
    assert!(imported.app().restore_held_cursor().is_none());
    assert!(w.rail.all_executions().is_empty());

    // A separately authorized payment uses the ordinary list request.
    let payment = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert_eq!(refusal(&payment), Refusal::WaitingForList);
    assert!(wallet
        .app()
        .sent_to(&request.payee, 0, u64::MAX)
        .await
        .unwrap()
        .iter()
        .any(|message| message.kind == "paykit.private_payment_list_request"));
}

#[tokio::test]
async fn test_period_payments_a_rate_quote_cannot_override_automatic_holds() {
    for under_allowance in [true, false] {
        let (w, _shop, mut wallet, request, _) =
            Box::pin(priced_period_on(false, store(), None)).await;
        let shown = crate::conversion::priced_of(
            &by_policy(&mut wallet, &request, january())
                .await
                .unwrap_err(),
        );
        if under_allowance {
            let mut record = request_record(&w, &wallet, &request).await;
            record.allowance_id = Some(
                paykit_lib::AllowanceId::parse("0b4f9c1e-8d2a-4e6b-9f3c-5a7d1e2b8c40").unwrap(),
            );
            write_payer_record(
                &mut wallet,
                &natural_key::request_record(&request.payee, &request.request_hash),
                &record,
            )
            .await;
        } else {
            let mut record = period_record(&w, &wallet, &request, january())
                .await
                .unwrap();
            record.manual_only = true;
            write_payer_record(&mut wallet, &period_natural(&request, january()), &record).await;
        }
        let refused = wallet
            .pay_period_automatically(&request, &january(), PeriodPrice::AtRate(&shown))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(refusal(&refused), Refusal::ManualOnly);
        assert!(wallet.payments().await.unwrap().is_empty());
        assert!(w.rail.all_executions().is_empty());
    }
}

#[tokio::test]
async fn test_period_payments_priced_repeat_requires_unexpired_selected_rates() {
    let (w, _shop, mut wallet, request, _) = Box::pin(priced_period_on(false, store(), None)).await;
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let shown = crate::conversion::priced_of(
        &by_user(&mut wallet, &request, january(), PayOptions::default())
            .await
            .unwrap_err(),
    );
    w.rail.next(Behaviour::Hang);
    let outcome = by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_rate_authorized(shown),
    )
    .await
    .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("{outcome:?}")
    };
    w.clock.advance_ms(120_000);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        outcomes.iter().any(|outcome| matches!(outcome,
        PayOutcome::Uncertain { attempt_id: id, .. } if *id == attempt_id)),
        "{outcomes:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_period_payments_priced_first_dispatch_rechecks_selected_rate_expiry() {
    for stage in ["attempted", "paying"] {
        let base = Arc::new(StageStore::default());
        let (w, _shop, mut wallet, request, _) = Box::pin(priced_period_on(
            false,
            Arc::clone(&base) as Arc<dyn StateStore>,
            None,
        ))
        .await;
        let shown = crate::conversion::priced_of(
            &by_user(&mut wallet, &request, january(), PayOptions::default())
                .await
                .unwrap_err(),
        );
        let clock = Arc::clone(&w.clock);
        base.on_stage(stage, move || clock.advance_ms(120_000));
        let error = by_user(
            &mut wallet,
            &request,
            january(),
            PayOptions::default().with_rate_authorized(shown),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(refusal(&error), Refusal::AttemptFailed(why)
            if matches!(*why, Refusal::PricedByRate { .. })),
            "{stage}: {error:?}"
        );
        assert!(w.rail.all_executions().is_empty(), "{stage}");
    }
}

#[tokio::test]
async fn test_period_payments_recorded_attempt_rechecks_the_period_rate_issuer() {
    let base = Arc::new(StageStore::default());
    let (w, shop, mut wallet, request, _) =
        Box::pin(priced_period_on(false, base.clone(), None)).await;
    let w = Arc::new(w);
    let mut quotes = w
        .join(
            &w.bob,
            "quotes",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    quotes.receive(None).await.unwrap();
    let sale = quotes.payee_activity().await.unwrap()[0].record;
    let quote = PeriodDetails {
        billing_period: january(),
        payment_endpoints: None,
        rates: Some(vec![paykit_lib::Rate::new(
            paykit_lib::Asset::new("btc").unwrap(),
            paykit_lib::Decimal::new("0.00002000").unwrap(),
        )
        .unwrap()]),
        expires_at: START_MS + HOUR_MS,
    };
    let id = quotes
        .send_period_details(&sale, quote)
        .await
        .unwrap()
        .msg_id;
    wallet.receive(None).await.unwrap();
    let shown = crate::conversion::priced_of(
        &by_user(
            &mut wallet,
            &request,
            january(),
            PayOptions::default().with_period_details(&id),
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(
        shown.source,
        paykit_sdk::RateSource::PeriodDetails { msg_id: id }
    );
    assert_eq!(shown.app_id, shop.app_id());

    // After planning, the wallet has recorded its confirmed choice. Its next
    // payee lookup sees the rate issuer removed while the endpoint app stays listed.
    let reader = Arc::clone(&w);
    let issuer = quotes.app_id();
    base.on_stage("recorded", move || {
        let now = reader.clock.now_ms();
        reader
            .bob
            .next_doc(|doc| {
                doc.apps.retain(|app| app.app_id != issuer);
                doc.removed.push(pubky_switch_core::records::RemovedApp {
                    app_id: issuer,
                    distrust_from: now,
                });
            })
            .unwrap();
        reader.clock.advance_ms(11 * 60_000);
    });
    let error = by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_rate_authorized(shown),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::AttemptFailed(reason)
        if matches!(reason.as_ref(), Refusal::Destination(why) if why.contains("sending app isn't current"))),
        "{error:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_period_payments_priced_repeat_preserves_selected_rate_and_amount() {
    let (w, _shop, mut wallet, request, _) = Box::pin(priced_period_on(false, store(), None)).await;
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let shown = crate::conversion::priced_of(
        &by_user(&mut wallet, &request, january(), PayOptions::default())
            .await
            .unwrap_err(),
    );
    w.rail.next(Behaviour::Hang);
    let outcome = by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_rate_authorized(shown),
    )
    .await
    .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("{outcome:?}")
    };
    w.clock.advance_ms(60_000);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        outcomes.iter().any(|outcome| matches!(outcome,
        PayOutcome::Pending { attempt_id: id, .. } if *id == attempt_id)),
        "{outcomes:?}"
    );
    assert_eq!(
        w.rail.pay_call_amounts(&attempt_id),
        vec![btc("0.00012346"), btc("0.00012346")]
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_period_payments_priced_default_uses_latest_details_carrying_rates() {
    let (w, mut shop, mut wallet, request, first) =
        Box::pin(priced_period_on(true, store(), None)).await;
    let confirmed = crate::conversion::priced_of(
        &by_user(&mut wallet, &request, january(), PayOptions::default())
            .await
            .unwrap_err(),
    );
    let sale = shop.payee_activity().await.unwrap()[0].record;
    w.clock.advance_ms(1_000);
    let mut body = details(january(), "unused", START_MS + 120_000);
    body.payment_endpoints = None;
    body.rates = Some(vec![paykit_lib::Rate::new(
        paykit_lib::Asset::new("btc").unwrap(),
        paykit_lib::Decimal::new("0.00002").unwrap(),
    )
    .unwrap()]);
    let latest = shop.send_period_details(&sale, body).await.unwrap().msg_id;
    wallet.receive(None).await.unwrap();
    let earlier = crate::conversion::priced_of(
        &by_user(
            &mut wallet,
            &request,
            january(),
            PayOptions::default().with_period_details(&first),
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(
        earlier.source,
        paykit_sdk::RateSource::PeriodDetails { msg_id: first }
    );
    assert_eq!(earlier.amount, btc("0.00012346"));
    assert_eq!(earlier, confirmed);
    let shown = crate::conversion::priced_of(
        &by_user(&mut wallet, &request, january(), PayOptions::default())
            .await
            .unwrap_err(),
    );
    assert_eq!(
        shown.source,
        paykit_sdk::RateSource::PeriodDetails { msg_id: latest }
    );
    assert_eq!(
        shown.payload_hash,
        pubky_switch_core::enc::hash(b"lnbc1invoice")
    );
    assert_eq!(shown.amount, btc("0.0002"));
    w.clock.advance_ms(1_000);
    let euro_details = PeriodDetails {
        billing_period: january(),
        payment_endpoints: Some(BTreeMap::from([(
            endpoint("eur-sepa-iban"),
            PaymentEndpointPayload::new("DE00iban").unwrap(),
        )])),
        rates: Some(vec![paykit_lib::Rate::new(
            paykit_lib::Asset::new("eur").unwrap(),
            paykit_lib::Decimal::new("0.92").unwrap(),
        )
        .unwrap()]),
        expires_at: START_MS + 120_000,
    };
    shop.send_period_details(&sale, euro_details).await.unwrap();
    wallet.receive(None).await.unwrap();
    let still_shown = crate::conversion::priced_of(
        &by_user(&mut wallet, &request, january(), PayOptions::default())
            .await
            .unwrap_err(),
    );
    assert_eq!(still_shown, shown);
    let outcome = by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_rate_authorized(confirmed),
    )
    .await
    .unwrap();
    assert_eq!(w.rail.executions(&outcome.attempt_id()), 1);
    assert_eq!(
        w.rail.paid_amount(&outcome.attempt_id()),
        Some(btc("0.00012346"))
    );
}

#[tokio::test]
async fn test_period_payments_held_rate_lasts_beyond_the_details_intake_window() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let rule = AnchoredRule::new(START_MS, 1, Unit::Minute).unwrap();
    let recurrence = Recurrence::new(rule, START_MS, None).unwrap();
    let first = PeriodRef::from(recurrence.first().unwrap());
    let draft =
        crate::conversion::priced(START_MS, &[("btc-lightning-bolt11", "lnbc1invoice")], &[])
            .with_conversion(paykit_lib::bodies::Conversion::PerPeriod)
            .with_recurrence(recurrence);
    let (request, sale) = subscribed_to(&w, &mut shop, &mut wallet, draft).await;
    let quote = PeriodDetails {
        billing_period: first,
        payment_endpoints: None,
        rates: Some(vec![paykit_lib::Rate::new(
            paykit_lib::Asset::new("btc").unwrap(),
            paykit_lib::Decimal::new("0.0000123456").unwrap(),
        )
        .unwrap()]),
        expires_at: START_MS + HOUR_MS,
    };
    let signed = shop
        .send_period_details(&sale, quote.clone())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();

    w.clock.advance_ms(16 * 60_000);
    let closed = shop.send_period_details(&sale, quote).await.unwrap_err();
    assert!(
        matches!(&closed, PaykitError::Lib(paykit_lib::Error::Validation(why)) if why.contains("16 latest")),
        "{closed:?}"
    );
    wallet.receive(None).await.unwrap();
    let shown = crate::conversion::priced_of(
        &by_user(&mut wallet, &request, first, PayOptions::default())
            .await
            .unwrap_err(),
    );
    assert_eq!(
        shown.source,
        paykit_sdk::RateSource::PeriodDetails {
            msg_id: signed.msg_id
        }
    );
    assert_eq!(shown.amount, btc("0.00012346"));
    let outcome = by_user(
        &mut wallet,
        &request,
        first,
        PayOptions::default().with_rate_authorized(shown),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }));
    assert_eq!(
        w.rail.paid_amount(&outcome.attempt_id()),
        Some(btc("0.00012346"))
    );
    assert_eq!(w.rail.pay_calls(&outcome.attempt_id()), 1);
}

#[tokio::test]
async fn test_period_payments_priced_terms_allow_requested_asset_without_details() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed_to(
        &w,
        &mut shop,
        &mut wallet,
        monthly().with_conversion(paykit_lib::bodies::Conversion::PerPeriod),
    )
    .await;
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert_eq!(w.rail.paid_amount(&outcome.attempt_id()), Some(btc("1")));
    assert!(wallet.payments().await.unwrap()[0].rate.is_none());

    let period = february();
    w.clock.set_ms(period.starts_at);
    let automatic = by_policy(&mut wallet, &request, period).await.unwrap();
    assert_eq!(w.rail.paid_amount(&automatic.attempt_id()), Some(btc("1")));
    assert_ne!(automatic.attempt_id(), outcome.attempt_id());
    assert_eq!(w.rail.all_executions().len(), 2);
    let payments = wallet.payments().await.unwrap();
    assert_eq!(payments.len(), 2);
    assert!(payments.iter().all(|payment| payment.rate.is_none()));
}

#[tokio::test]
async fn test_period_payments_cross_asset_without_terms_requires_manual_payment() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut draft = monthly();
    draft.amount.asset = paykit_lib::Asset::new("usd").unwrap();
    let (request, _) = subscribed_to(&w, &mut shop, &mut wallet, draft).await;
    let automatic = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert!(
        matches!(automatic.refusal(), Some(Refusal::UnsupportedEndpoint(why)) if why.contains("only by hand")),
        "{automatic:?}"
    );
    let manual = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(manual.refusal(), Some(Refusal::Transfer { reason, .. }) if reason.contains("only by hand")),
        "{manual:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
}

/// A hook [`StageStore`] runs once.
type StageHook = Box<dyn FnOnce() + Send>;

/// A store that runs a hook once, right after the commit of the first journal write at a
/// stage.
#[derive(Default)]
struct StageStore {
    inner: MemoryStore,
    hook: std::sync::Mutex<Option<(&'static str, StageHook)>>,
}

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
    ) -> Result<Vec<KeyValue>, pubky_switch_client::error::StoreError> {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(&self, batch: WriteBatch) -> Result<(), pubky_switch_client::error::StoreError> {
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_switch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
    ) -> Result<pubky_switch_client::store::AcquireOutcome, pubky_switch_client::error::StoreError>
    {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
        batch: WriteBatch,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}

impl StageStore {
    /// Apply `batch` with `apply`, then run the hook armed for a journal write at its stage,
    /// once.
    fn commit_with(
        &self,
        batch: WriteBatch,
        apply: impl FnOnce(
            WriteBatch,
        ) -> Result<
            pubky_switch_client::store::CommitOutcome,
            pubky_switch_client::error::StoreError,
        >,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        let armed = self.hook.lock().unwrap().as_ref().map(|(stage, _)| *stage);
        let due = armed.is_some_and(|stage| {
            let marker = format!("\"stage\":\"{stage}\"");
            batch.writes().iter().any(|write| {
                matches!(write, Write::Put { key, value, .. }
                    if key.starts_with(b"p/paykit/j/")
                        && value.windows(marker.len()).any(|w| w == marker.as_bytes()))
            })
        });
        let outcome = apply(batch)?;
        let hook = if due {
            self.hook.lock().unwrap().take()
        } else {
            None
        };
        if let Some((_, hook)) = hook {
            hook();
        }
        Ok(outcome)
    }
}

#[tokio::test]
async fn test_period_payments_payee_cancellation_crosses_an_accepted_attempt() {
    Box::pin(seller_stop_after_attempt(StopAt::Manual)).await;
}

#[tokio::test]
async fn test_period_payments_seller_stop_crosses_the_first_dispatch_by_policy() {
    Box::pin(seller_stop_after_attempt(StopAt::Policy)).await;
}

#[tokio::test]
async fn test_period_payments_seller_stop_between_policy_reads_finishes_the_open_attempt() {
    Box::pin(seller_stop_after_attempt(StopAt::PolicyRead)).await;
}

#[tokio::test]
async fn test_period_payments_seller_stop_between_policy_reads_keeps_an_independent_mark() {
    Box::pin(seller_stop_after_attempt(StopAt::PolicyReadMarked)).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StopAt {
    Manual,
    Policy,
    PolicyRead,
    PolicyReadMarked,
}

async fn seller_stop_after_attempt(stop_at: StopAt) {
    let w = World::new();
    let base = Arc::new(StageStore::default());
    let reads = Arc::new(crate::common::ReadHook::new(w.router.clone()));
    let mut payer = Box::pin(w.join_through(
        &w.alice,
        "phone",
        paykit_sdk::AppScopes::new().pay().build().unwrap(),
        PaykitConfig::default(),
        base.clone(),
        reads.clone(),
    ))
    .await;
    payer.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut tablet = w.wallet("tablet").await;
    let mut shop = w.shop().await;
    let (request, sale) = subscribed(&w, &mut shop, &mut payer).await;
    tablet.receive(None).await.unwrap();
    let obligation = paykit_lib::derive::request_obligation(
        payer.app().naming_key(),
        &request.payee,
        &request.request_hash,
        january().start_secs(),
    );
    let rail = Arc::clone(&w.rail);
    let (finished, seller) = std::sync::mpsc::channel();
    let cancel = move || {
        // Both apps use their ordinary APIs while the phone is between its commit and call.
        let shop = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async move {
                    let claim = tablet
                        .app_mut()
                        .read_claim(&obligation)
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(claim.view.status, ClaimStatus::Attempting);
                    assert!(rail.all_executions().is_empty());
                    shop.cancel_sent_request(&sale, None).await.unwrap();
                    tablet.receive(None).await.unwrap();
                    assert_eq!(
                        tablet.request(&request).await.unwrap().status,
                        RequestStatus::Cancelled
                    );
                    if stop_at == StopAt::PolicyReadMarked {
                        let natural =
                            natural_key::request_record(&request.payee, &request.request_hash);
                        let key = tablet.app().record_key(&natural_target(&natural)).unwrap();
                        let current = tablet.app().shared_record(&key).await.unwrap().unwrap();
                        let mut record = ActivityRecord::bind(
                            current.content.value().unwrap(),
                            &tablet.identity(),
                            ActivitySide::Payer,
                            tablet.app().naming_key(),
                            &key,
                            None,
                        )
                        .unwrap();
                        record.manual_only = true;
                        write_payer_record(&mut tablet, &natural, &record).await;
                    }
                    shop
                })
        })
        .join()
        .unwrap();
        assert!(finished.send(shop).is_ok());
    };
    let parent = natural_target(&natural_key::request_record(
        &request.payee,
        &request.request_hash,
    ));
    let path = format!(
        "/records/{}",
        pubky_switch_core::enc::b64(&payer.app().record_key(&parent).unwrap())
    );
    if matches!(stop_at, StopAt::PolicyRead | StopAt::PolicyReadMarked) {
        base.on_stage("paying", move || {
            let next = reads.clone();
            reads.after_read("/allow", move || next.after_read(&path, cancel));
        });
    } else {
        base.on_stage("paying", cancel);
    }
    let paid = if stop_at == StopAt::Manual {
        by_user(&mut payer, &request, january(), PayOptions::default()).await
    } else {
        by_policy(&mut payer, &request, january()).await
    }
    .unwrap();
    if stop_at == StopAt::PolicyReadMarked {
        assert!(matches!(paid, PayOutcome::Pending { .. }), "{paid:?}");
        assert_eq!(w.rail.pay_calls(&paid.attempt_id()), 0);
        assert!(period_record(&w, &payer, &request, january())
            .await
            .unwrap()
            .proof
            .is_none());
        assert!(seller.try_recv().is_ok());
        return;
    }
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.pay_calls(&paid.attempt_id()), 1);
    let record = period_record(&w, &payer, &request, january())
        .await
        .unwrap();
    let (proof, body) = proof_of(&record);
    assert_eq!(body.billing_period, Some(january()));
    assert_eq!(body.attempt_id, paid.attempt_id());
    assert!(body.allowance_id.is_none());
    assert_eq!(proof.message().refs, [request.request_hash]);
    let mut shop = seller.try_recv().unwrap();
    shop.receive(None).await.unwrap();
    let period = &shop.payee_activity().await.unwrap()[0].periods[0];
    assert_eq!(period.billing_period, january());
    let received = shop
        .app()
        .shared_record(&period.record)
        .await
        .unwrap()
        .unwrap();
    let record = ActivityRecord::read(
        received.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payee,
    )
    .unwrap();
    assert_eq!(proof_of(&record).0.hash(), proof.hash());
    payer.receive(None).await.unwrap();
    let next = if stop_at == StopAt::Manual {
        by_user(&mut payer, &request, february(), early()).await
    } else {
        by_policy(&mut payer, &request, february()).await
    }
    .unwrap_err();
    assert_eq!(refusal(&next), Refusal::Cancelled);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_period_payments_period_record_divergent_after_the_paying_commit_holds_it() {
    // Right after the wallet journals January's `Paying` by hand, a view shows January's
    // period record divergent: the dispatch is held, nothing paid, as the check before it
    // judges every pause after its last read (Paykit spec 6).
    let w = World::new();
    let base = Arc::new(StageStore::default());
    let mut shop = w.shop().await;
    let mut wallet = w
        .wallet_on("wallet", Arc::clone(&base) as Arc<dyn StateStore>)
        .await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let record = period_key(&wallet, &request, january());
    let store = Arc::clone(&base) as Arc<dyn StateStore>;
    let app = wallet.app_id();
    base.on_stage("paying", move || {
        let ns = pubky_switch_client::store::client_namespace(&app);
        let key = format!("rcdiv/{}", pubky_switch_core::enc::b64(&record));
        let mut batch = WriteBatch::new();
        batch
            .put(&ns, key.into_bytes(), br#"{"v":3,"d":null}"#.to_vec())
            .unwrap();
        store.commit(batch).unwrap();
    });
    let outcome = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_period_payments_unknown_restore_holds_until_the_user_confirms() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restore_before_first_read(&w, &backup).await;
    crate::common::restore_with_unknown_cursor(&mut app).await;
    app.receive(None).await.unwrap();
    assert_eq!(
        app.app().restore_held_cursor(),
        Some(pubky_switch_client::app::HeldCursor::Unknown)
    );
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    let refused = by_user(&mut app, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refusal(&refused), Refusal::HeldAfterRestore { .. }),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let confirmed = crate::common::over_hold(&app, &request).await;
    let outcome = by_user(&mut app, &request, january(), confirmed)
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(w.rail.executions(&outcome.attempt_id()), 1);
}

#[tokio::test]
async fn test_period_payments_clean_restore_pays_without_hold_confirmation() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    assert_eq!(
        app.app().restore_held_cursor(),
        Some(pubky_switch_client::app::HeldCursor::Clean)
    );
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    let outcome = by_user(&mut app, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(w.rail.executions(&outcome.attempt_id()), 1);
}

/// A wallet on an idempotent rail without history pays January of bob's subscription by
/// hand, the dispatch left in flight; then `between` runs with the shop and the shop's record
/// of the sale, and the wallet resumes a minute later. Returns how many times the rail was
/// called, and the resume's outcome of the payment.
async fn resumed_period_repeat(
    w: &World,
    between: impl AsyncFnOnce(&mut Paykit, [u8; 32]),
) -> (u64, PayOutcome) {
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
    let resumed = wallet
        .resume_payments()
        .await
        .unwrap()
        .into_iter()
        .find(|o| o.attempt_id() == pending.attempt_id())
        .unwrap();
    (w.rail.pay_calls(&pending.attempt_id()), resumed)
}

#[tokio::test]
async fn test_period_payments_resume_repeats_a_periods_dispatch_unless_cancelled_since() {
    // A crash leaves January's dispatch in flight: the resume repeats it on the idempotent
    // method, its period record's mark from the user's own attempt withholding nothing; once
    // the payee has cancelled the subscription meanwhile, it isn't repeated, and the attempt
    // stays "may be paid" (Paykit spec 7.2, 8.2, 8.3).
    let w = World::new();
    let (calls, _) = resumed_period_repeat(&w, async |_, _| {}).await;
    assert_eq!(calls, 2);
    let w = World::new();
    let (calls, _) = resumed_period_repeat(&w, async |shop, sale| {
        shop.cancel_sent_request(&sale, None).await.unwrap();
    })
    .await;
    assert_eq!(calls, 1);
}

async fn resume_marked_subscription(
    w: &World,
    idempotent: bool,
    after_mark: impl AsyncFnOnce(&mut Paykit, [u8; 32]),
) -> (paykit_lib::AttemptId, Vec<PayOutcome>, ActivityRecord) {
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(idempotent),
    );
    let storage = store();
    let mut wallet = w
        .wallet_with("wallet", storage.clone(), crate::common::day_window())
        .await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (request, sale) = subscribed(w, &mut shop, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    let pending = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }), "{pending:?}");
    let attempt = pending.attempt_id();
    let app_id = wallet.app_id();
    drop(wallet);

    phone.receive(None).await.unwrap();
    let mut parent = request_record(w, &phone, &request).await;
    parent.manual_only = true;
    write_payer_record(
        &mut phone,
        &natural_key::request_record(&request.payee, &request.request_hash),
        &parent,
    )
    .await;
    after_mark(&mut shop, sale).await;
    // The method settles the first dispatch and has no history lookup.
    w.rail.settle(&attempt, true);
    w.clock.advance_ms(60_000);
    let mut wallet = w
        .load_with(
            &w.alice,
            storage,
            app_id,
            w.clock.clone(),
            crate::common::day_window(),
        )
        .await;
    wallet.receive(None).await.unwrap();
    assert!(request_record(w, &wallet, &request).await.manual_only);
    let outcomes = wallet.resume_payments().await.unwrap();
    let period = period_record(w, &wallet, &request, january())
        .await
        .unwrap();
    (attempt, outcomes, period)
}

#[tokio::test]
async fn test_period_payments_resume_the_users_attempt_on_a_manual_only_subscription() {
    let w = World::new();
    let (attempt, outcomes, paid) = resume_marked_subscription(&w, true, async |_, _| {}).await;
    assert!(
        matches!(outcomes.as_slice(), [PayOutcome::Paid { attempt_id, .. }] if *attempt_id == attempt),
        "{outcomes:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt), 2);
    assert_eq!(w.rail.all_executions().values().sum::<u64>(), 1);
    assert!(paid.manual_only);
    let (_, proof) = proof_of(&paid);
    assert_eq!(proof.attempt_id, attempt);
    assert_eq!(proof.billing_period, Some(january()));
}

fn assert_period_stays_uncertain(
    w: &World,
    (attempt, outcomes, period): (paykit_lib::AttemptId, Vec<PayOutcome>, ActivityRecord),
) {
    assert!(
        matches!(outcomes.as_slice(), [PayOutcome::Uncertain { attempt_id, .. }] if *attempt_id == attempt),
        "{outcomes:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt), 1);
    assert_eq!(w.rail.all_executions().values().sum::<u64>(), 1);
    assert!(period.proof.is_none());
}

#[tokio::test]
async fn test_period_payments_hold_a_cancelled_attempt_on_a_manual_only_subscription() {
    let w = World::new();
    let resumed = resume_marked_subscription(&w, true, async |shop, sale| {
        shop.cancel_sent_request(&sale, None).await.unwrap();
    })
    .await;
    assert_period_stays_uncertain(&w, resumed);
}

#[tokio::test]
async fn test_period_payments_hold_a_dispatch_without_idempotency_or_history() {
    let w = World::new();
    let resumed = resume_marked_subscription(&w, false, async |_, _| {}).await;
    assert_period_stays_uncertain(&w, resumed);
}

#[tokio::test]
async fn test_period_payments_reconcile_holds_without_selected_period_rate() {
    let (w, _shop, mut wallet, request, _) = Box::pin(priced_period_on(false, store(), None)).await;
    let backup = wallet.backup().await.unwrap();
    let shown = crate::conversion::priced_of(
        &by_user(&mut wallet, &request, january(), PayOptions::default())
            .await
            .unwrap_err(),
    );
    w.rail.next(Behaviour::Hang);
    let attempt = by_user(
        &mut wallet,
        &request,
        january(),
        PayOptions::default().with_rate_authorized(shown),
    )
    .await
    .unwrap()
    .attempt_id();
    assert_eq!(w.rail.paid_amount(&attempt), Some(btc("0.00012346")));
    w.rail.settle(&attempt, true);
    drop(wallet);
    let mut app = Box::pin(restored(&w, &backup)).await;
    let report = app.reconcile().await.unwrap();
    assert_eq!(report.doubtful.len(), 1, "{report:?}");
    assert_eq!(report.doubtful[0].attempts, [attempt]);
    assert!(
        !report
            .resolved
            .iter()
            .any(|outcome| matches!(outcome, PayOutcome::Paid { .. })),
        "{report:?}"
    );
    assert!(app.payments().await.unwrap().is_empty());
    let claim = app
        .app_mut()
        .read_claim(&report.doubtful[0].obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Attempting);
    assert_eq!(w.rail.executions(&attempt), 1);
    assert_eq!(w.rail.pay_calls(&attempt), 1);
}

#[tokio::test]
async fn test_period_payments_reconcile_finds_a_period_paid_after_the_backup() {
    // February is paid after the backup and the device lost before its outcome: the restored
    // app finds the attempt on the period the request record lists, from the payment
    // method's history, and writes its outcome into the period record (Paykit spec 7.3, 8.2).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let schedule = fractional_monthly();
    let period = PeriodRef::from(schedule.periods(2).unwrap()[1]);
    let (request, _) = subscribed_to(
        &w,
        &mut shop,
        &mut wallet,
        monthly().with_recurrence(schedule),
    )
    .await;
    let backup = wallet.backup().await.unwrap();
    w.rail.next(Behaviour::Hang);
    let attempt = by_user(&mut wallet, &request, period, early())
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
            .any(|o| matches!(o, PayOutcome::Paid { attempt_id, .. } if *attempt_id == attempt)),
        "{report:?}"
    );
    let record = period_record(&w, &app, &request, period).await.unwrap();
    assert_eq!(proof_of(&record).1.billing_period, Some(period));
    assert!(record.manual_only);
    assert_eq!(
        app.payments().await.unwrap()[0].billing_period,
        Some(period)
    );
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap()[0].periods[0].paid);
    assert_eq!(w.rail.executions(&attempt), 1);
}

#[tokio::test]
async fn test_period_payments_reconcile_leaves_a_period_whose_destination_it_cant_tell() {
    // With `per_period_endpoints`, a later period is paid to its period details'
    // destination, which the records don't tell: an own attempt on February the journal
    // doesn't hold, paid by the method, stays doubtful after a restore, never rebuilt with
    // the request's endpoint (Paykit spec 7.3, 8.2).
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let schedule = fractional_monthly();
    let period = PeriodRef::from(schedule.periods(2).unwrap()[1]);
    let (request, _) = subscribed_to(
        &w,
        &mut shop,
        &mut wallet,
        monthly()
            .with_recurrence(schedule)
            .with_per_period_endpoints(),
    )
    .await;
    let backup = wallet.backup().await.unwrap();
    // The wallet's attempt on February, as another version's batch would write it: the
    // period listed and its claim's attempt; then the method pays its details' destination.
    w.clock.set_ms(period.starts_at);
    let natural = natural_key::request_record(&request.payee, &request.request_hash);
    let mut record = request_record(&w, &wallet, &request).await;
    record.attempted_periods.push(period.start_secs());
    write_payer_record(&mut wallet, &natural, &record).await;
    let obligation = paykit_lib::derive::request_obligation(
        wallet.app().naming_key(),
        &request.payee,
        &request.request_hash,
        period.start_secs(),
    );
    let attempt = paykit_lib::AttemptId::generate().unwrap();
    let op =
        pubky_switch_client::app::ClaimOp::attempt(obligation, 0, attempt.to_bytes(), 4200, None);
    wallet
        .app_mut()
        .submit_batch(
            OpBatch::for_protocol("paykit")
                .op(op.kind())
                .obligation_deadline(
                    obligation,
                    pubky_switch_client::app::ObligationDeadline::NoDeadline,
                ),
        )
        .await
        .unwrap();
    w.rail
        .pay_outside(
            endpoint("btc-lightning-bolt11"),
            PaymentEndpointPayload::new("lnbc1feb").unwrap(),
            btc("1"),
            attempt,
        )
        .await
        .unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    let doubtful = report
        .doubtful
        .iter()
        .find(|d| d.obligation == obligation)
        .unwrap_or_else(|| panic!("{report:?}"));
    assert_eq!(doubtful.attempts, [attempt]);
    assert_eq!(doubtful.payee, Some(request.payee));
    assert_eq!(doubtful.request_hash, Some(request.request_hash));
    assert!(report.resolved.is_empty(), "{report:?}");
    assert!(period_record(&w, &app, &request, period).await.is_none());
}

/// A backup `w`'s wallet takes paying January by hand, its dispatch in flight.
async fn backup_paying_january(w: &World) -> serde_json::Value {
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(w, &mut shop, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Whether a restore takes `backup`, its Paykit layer's journal entries rewritten by
/// `tamper`.
async fn restore_takes(
    w: &World,
    backup: &serde_json::Value,
    tamper: impl Fn(&mut serde_json::Value),
) -> bool {
    let data = backup["layers"][0]["data"].as_str().unwrap();
    let bytes = pubky_switch_core::enc::unb64(data).unwrap();
    let mut state: Vec<(Vec<u8>, Vec<u8>)> = pubky_switch_client::app::decode_local_state(&bytes)
        .unwrap()
        .into_iter()
        .collect();
    let mut found = false;
    for (key, value) in &mut state {
        if key.starts_with(b"j/") {
            found = true;
            let mut json: serde_json::Value = serde_json::from_slice(value).unwrap();
            tamper(&mut json["d"]);
            *value = serde_json::to_vec(&json).unwrap();
        }
    }
    assert!(found, "no journal entry");
    let mut out = backup.clone();
    out["layers"][0]["data"] = serde_json::Value::String(pubky_switch_core::enc::b64(
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

type Tamper = dyn Fn(&mut serde_json::Value);

#[tokio::test]
async fn test_period_payments_restore_refuses_a_period_entry_no_run_writes() {
    // A period's journal entry whose period isn't one of its request's Billing Periods,
    // whose deadline isn't that period's attempt deadline, that names another period's
    // claim, owes an acceptance, sits at its request record's key, has an empty period, or
    // names no period at all: no run writes one, and the restore refuses the backup; the
    // entry as written restores (Paykit spec 7.1, 8.2).
    let w = World::new();
    let backup = backup_paying_january(&w).await;
    assert!(
        restore_takes(&w, &backup, |entry| {
            assert_eq!(entry["user_started"], true);
        })
        .await
    );
    assert!(
        !restore_takes(&w, &backup, |entry| {
            entry.as_object_mut().unwrap().remove("user_started");
        })
        .await
    );
    let request_key = |e: &serde_json::Value| {
        let payee =
            pubky_switch_core::enc::unb64_array::<32>(e["payee"].as_str().unwrap()).unwrap();
        let hash =
            pubky_switch_core::enc::unb64_array::<32>(e["request_hash"].as_str().unwrap()).unwrap();
        pubky_switch_core::enc::b64(&natural_key::request_record(&payee, &hash))
    };
    let shifted = |e: &mut serde_json::Value, member: &str, by: u64| {
        let at = e["billing_period"][member].as_u64().unwrap();
        e["billing_period"][member] = serde_json::json!(at + by);
    };
    let refused: [Box<Tamper>; 7] = [
        Box::new(move |e| shifted(e, "starts_at", 1)),
        Box::new(move |e| shifted(e, "ends_at", 1000)),
        Box::new(|e| e["deadline"] = serde_json::json!(START_MS + DAY_MS)),
        Box::new(|e| e["obligation"] = serde_json::json!(pubky_switch_core::enc::b64(&[7; 32]))),
        Box::new(|e| e["acceptance_owed"] = serde_json::json!(true)),
        Box::new(move |e| e["natural_key"] = serde_json::json!(request_key(e))),
        Box::new(|e| e["billing_period"]["ends_at"] = e["billing_period"]["starts_at"].clone()),
    ];
    for tamper in refused {
        assert!(!restore_takes(&w, &backup, tamper).await);
    }
    // Naming no period, at the request record's key and on Billing Period 0's claim, as a
    // one-time request's entry is: a recurring request's payment pays one of its periods.
    let naming = w.wallet("other").await.app().naming_key().clone();
    let unperiodic = move |e: &mut serde_json::Value| {
        let payee =
            pubky_switch_core::enc::unb64_array::<32>(e["payee"].as_str().unwrap()).unwrap();
        let hash =
            pubky_switch_core::enc::unb64_array::<32>(e["request_hash"].as_str().unwrap()).unwrap();
        e["billing_period"] = serde_json::Value::Null;
        e["natural_key"] = serde_json::json!(pubky_switch_core::enc::b64(
            &natural_key::request_record(&payee, &hash)
        ));
        let obligation = paykit_lib::derive::request_obligation(&naming, &payee, &hash, 0);
        e["obligation"] = serde_json::json!(pubky_switch_core::enc::b64(&obligation));
    };
    assert!(!restore_takes(&w, &backup, unperiodic).await);
    // An entry without the member at all is refused too: it is required.
    assert!(
        !restore_takes(&w, &backup, |e| {
            e.as_object_mut().unwrap().remove("billing_period");
        })
        .await
    );
}

#[tokio::test]
async fn test_period_payments_restore_refuses_a_period_in_a_payment_without_a_request() {
    // A payment without a request pays no Billing Period: its entry naming one is refused,
    // and so is one without the member, which every entry carries; the entry as written
    // restores (Paykit spec 8.4; Paykit wire 4.10).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id()],
        crate::common::list_version(1, 1),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let payment = paykit_sdk::UnrequestedPayment::new(w.bob_id(), btc("0.0015"))
        .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")));
    let paid = wallet.pay_without_request(payment).await.outcome.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    let backup: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(restore_takes(&w, &backup, |_| {}).await);
    assert!(
        !restore_takes(&w, &backup, |e| {
            e["billing_period"] = serde_json::json!({
                "starts_at": january().starts_at,
                "ends_at": january().ends_at,
            });
        })
        .await
    );
    assert!(
        !restore_takes(&w, &backup, |e| {
            e.as_object_mut().unwrap().remove("billing_period");
        })
        .await
    );
}

#[tokio::test]
async fn test_period_payments_prior_subscription_keeps_future_periods_in_doubt_after_loss() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    w.rail.next(Behaviour::Fail);
    let failed = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(failed, PayOutcome::Failed { .. }), "{failed:?}");
    let executions = w.rail.all_executions();
    w.clock.advance_ms(60_000);
    crate::provider_loss::declare_loss(&w);
    let until = crate::provider_loss::follow_loss(&mut wallet).await;
    assert!(until < february().starts_at);

    let january_refused = by_user(&mut wallet, &request, january(), PayOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(refusal(&january_refused),
        Refusal::MayBePaid { reason, doubt: None } if reason.contains("provider loss")));
    let obligation = |start| {
        paykit_lib::derive::request_obligation(
            wallet.app().naming_key(),
            &request.payee,
            &request.request_hash,
            start,
        )
    };
    let jan_key = obligation(january().start_secs());
    let feb_key = obligation(february().start_secs());
    assert_ne!(jan_key, feb_key);
    let report = wallet.reconcile().await.unwrap();
    wallet.end_reconcile(UserConfirmed::new()).await.unwrap();
    let loss = report
        .doubtful
        .iter()
        .find(|d| d.obligation == jan_key)
        .and_then(|d| d.before_loss)
        .expect("the January occurrence reports the provider loss");
    let february_refused = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap_err();
    assert!(matches!(refusal(&february_refused),
        Refusal::MayBePaid { reason, doubt: None } if reason.contains("provider loss")));
    wallet
        .confirm_not_paid_before_loss(&feb_key, &loss, UserConfirmed::new())
        .await
        .unwrap();
    let retry = by_user(&mut wallet, &request, february(), early())
        .await
        .unwrap_err();
    assert_eq!(refusal(&retry), refusal(&february_refused));
    assert_eq!(w.rail.all_executions(), executions);
}

#[tokio::test]
async fn test_period_payments_by_policy_pays_when_every_automatic_rule_holds() {
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    let early = by_policy(&mut wallet, &request, february())
        .await
        .unwrap_err();
    assert_eq!(refusal(&early), Refusal::PeriodNotStarted);
    assert!(w.rail.all_executions().is_empty());
    let outcome = by_policy(&mut wallet, &request, january()).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let record = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert!(!record.manual_only);
    let proof = proof_of(&record).1;
    assert_eq!(proof.billing_period, Some(january()));
    assert_eq!(proof.attempt_id, outcome.attempt_id());
    assert!(proof.allowance_id.is_none());
    assert_eq!(w.rail.executions(&proof.attempt_id), 1);
    let repeated = by_policy(&mut wallet, &request, january())
        .await
        .unwrap_err();
    assert_eq!(refusal(&repeated), Refusal::AlreadyPaid);
    assert_eq!(w.rail.pay_calls(&proof.attempt_id), 1);
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
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut manager = w
        .join(
            &w.alice,
            "manager",
            paykit_sdk::AppScopes::new()
                .pay()
                .manage_contacts()
                .build()
                .unwrap(),
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
    manager.unblock(&w.bob_id()).await.unwrap();
    let outcome = by_policy(&mut wallet, &request, january()).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_period_payments_by_policy_resume_honors_a_new_manual_only_mark() {
    let w = World::new();
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let mut shop = w.shop().await;
    let mut wallet = w
        .wallet_with("wallet", store(), crate::common::day_window())
        .await;
    let mut phone = w.wallet("phone").await;
    let (request, _) = subscribed(&w, &mut shop, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    let pending = by_policy(&mut wallet, &request, january()).await.unwrap();
    assert!(matches!(pending, PayOutcome::Pending { .. }));
    phone.receive(None).await.unwrap();
    let mut marked = period_record(&w, &phone, &request, january())
        .await
        .unwrap();
    marked.manual_only = true;
    write_payer_record(&mut phone, &period_natural(&request, january()), &marked).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    let resumed = wallet.resume_payments().await.unwrap();
    let period = period_record(&w, &wallet, &request, january())
        .await
        .unwrap();
    assert_period_stays_uncertain(&w, (pending.attempt_id(), resumed, period));
}

#[tokio::test]
async fn test_period_payments_by_policy_stops_at_the_earlier_attempt_deadline() {
    for after_deadline_ms in [0, 1] {
        let w = World::new();
        let mut shop = w.shop().await;
        let mut wallet = w.wallet("wallet").await;
        let (request, _) = subscribed_to(
            &w,
            &mut shop,
            &mut wallet,
            monthly().with_period_deadline_s(60),
        )
        .await;
        w.clock
            .set_ms(january().starts_at + 60_000 + after_deadline_ms);
        let payment = by_policy(&mut wallet, &request, january()).await;
        if after_deadline_ms == 1 {
            let error = payment.unwrap_err();
            assert_eq!(refusal(&error), Refusal::DeadlinePassed);
            assert!(w.rail.all_executions().is_empty());
        } else {
            let outcome = payment.unwrap();
            assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
            assert_eq!(w.rail.pay_calls(&outcome.attempt_id()), 1);
        }
    }
}
