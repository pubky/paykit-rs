//! What a payment under an Allowance is refused with when it may not start: a state no later
//! read changes refuses for good, one the user lifts names it, and only a state a later read
//! or answer can change refuses as "not now" (Paykit spec 8.3, "Executor", "Ends reach the
//! executor"). A host's scheduler stops on the first and asks the user on the second.

use std::sync::Arc;

use paykit_lib::allowances::{AllowanceTerms, TermsTime};
use paykit_lib::bodies::RequestDestination;
use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
use paykit_lib::{Asset, Decimal, PaymentAmount, PaymentEndpointPayload, PaymentReference};
use paykit_sdk::rail::{Behaviour, Rail};
use paykit_sdk::{
    AllowanceStatus, AppScopes, PayOptions, PayOutcome, PayResult, Paykit, PaykitConfig, Refusal,
    RequestDraft, RequestRef,
};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;
use serde_json::json;

use super::{first_grant_as, first_grant_in, terms};
use crate::common::{draft, endpoint, request_from, store, World, HOUR_MS};

/// The refusal a payment's result carries.
fn refused(result: PayResult) -> Refusal {
    match result.outcome {
        Err(error) => error
            .refusal()
            .cloned()
            .unwrap_or_else(|| panic!("not a refusal: {error:?}")),
        Ok(outcome) => panic!("paid: {outcome:?}"),
    }
}

/// The refusal a call returned.
fn refusal_of<T: std::fmt::Debug>(result: paykit_sdk::Result<T>) -> Refusal {
    match result {
        Err(error) => error
            .refusal()
            .cloned()
            .unwrap_or_else(|| panic!("not a refusal: {error:?}")),
        Ok(value) => panic!("not refused: {value:?}"),
    }
}

/// Bob's shop sends alice a one-time request for `value` btc, which her wallet receives.
async fn one_time(w: &World, shop: &mut Paykit, payer: &mut Paykit, value: &str) -> RequestRef {
    let request = Box::pin(request_from(
        shop,
        w.alice_id(),
        draft(value, w.clock.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    request
}

/// Bob's shop sends alice an hourly request for 1 btc, which her wallet accepts under the
/// Allowance at `key`. Returns the request and its first two periods.
async fn subscribed(
    w: &World,
    shop: &mut Paykit,
    payer: &mut Paykit,
    key: &[u8; 32],
) -> (RequestRef, [PeriodRef; 2]) {
    let start = w.clock.now_ms() / 1_000 * 1_000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let periods = schedule.periods(2).unwrap();
    let request = Box::pin(request_from(
        shop,
        payer.identity(),
        draft("1", start).with_recurrence(schedule),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    Box::pin(payer.accept_subscription_under_allowance(&request, key))
        .await
        .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    (
        request,
        [PeriodRef::from(periods[0]), PeriodRef::from(periods[1])],
    )
}

/// The terms of [`terms`], with `changes` made.
fn terms_with(changes: serde_json::Value) -> AllowanceTerms {
    let mut json = terms().to_json().unwrap();
    for (key, value) in changes.as_object().unwrap() {
        json[key] = value.clone();
    }
    AllowanceTerms::from_json(&json).unwrap()
}

/// A temporary refusal, with its reason.
fn not_now(reason: &str) -> Refusal {
    Refusal::NotNow(reason.into())
}

/// Bob's app sends alice's wallet a second, different proposal of the Allowance at `key`,
/// which conflicts it.
async fn conflict(w: &World, payer: &mut Paykit, key: &[u8; 32]) {
    let id = payer.allowance(key).await.unwrap().allowance_id;
    let mut other = terms().to_json().unwrap();
    other["lifetime_amount_limit"] = json!("0.20");
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[payer.app_id()],
        "paykit.allowance_proposal",
        Vec::new(),
        json!({ "allowance_id": id.as_str(), "proposer_role": "allowee", "allower_generation": 1, "terms": other })
            .as_object()
            .cloned()
            .unwrap(),
    );
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(
        payer.allowance(key).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
}

/// A `missed` item at alice's switch, read: every Allowance on the wallet's consent list
/// waits for the user's confirmation again.
async fn missed(w: &World, payer: &mut Paykit) {
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    Box::pin(crate::common::finish_listing(payer)).await;
}

/// The wallet confirms the hold of `request` if it is held, as the host does once the user
/// has seen the request may have been cancelled.
async fn confirm_hold(payer: &mut Paykit, request: &RequestRef) {
    if let Some(held) = payer.request(request).await.unwrap().held {
        Box::pin(payer.confirm_possibly_cancelled(request, &held, UserConfirmed::new()))
            .await
            .unwrap();
    }
}

/// A new one-time request, its hold confirmed, paid under the Allowance at `key`.
async fn pays(w: &World, shop: &mut Paykit, payer: &mut Paykit, key: &[u8; 32]) {
    let request = Box::pin(one_time(w, shop, payer, "1")).await;
    Box::pin(confirm_hold(payer, &request)).await;
    let paid = Box::pin(payer.pay_under_allowance(&request, key))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

/// The shop's record of the one Allowance it holds.
async fn shop_record(shop: &Paykit) -> [u8; 32] {
    shop.allowances().await.unwrap().pop().unwrap().record
}

#[tokio::test]
async fn test_allowance_refusals_end_is_final_at_every_scheduled_retry() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    let (request, periods) = Box::pin(subscribed(&w, &mut shop, &mut payer, &key)).await;
    let paid = Box::pin(payer.pay_period_under_allowance(&request, &periods[0]))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    Box::pin(payer.end_allowance(&key)).await.unwrap();

    // A scheduler that fires again and again gets the final refusal every time, never a
    // retryable one, and nothing more is paid.
    w.clock.set_ms(periods[1].starts_at + 1_000);
    for _ in 0..4 {
        let refusal =
            refused(Box::pin(payer.pay_period_under_allowance(&request, &periods[1])).await);
        assert_eq!(refusal, Refusal::AllowanceEnded);
        w.clock.advance_ms(60_000);
    }
    let later = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&later, &key)).await),
        Refusal::AllowanceEnded
    );
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        Refusal::AllowanceEnded
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_allowance_refusals_counterparty_end_is_final() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    let record = shop_record(&shop).await;
    Box::pin(shop.end_allowance(&record)).await.unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::AllowanceEnded
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_conflict_is_final() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    Box::pin(conflict(&w, &mut payer, &key)).await;
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::AllowanceConflicted
    );
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        Refusal::AllowanceConflicted
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_expired_terms_end_it() {
    let start = pubky_switch_client::testing::START_MS;
    let mut expiring = terms().to_json().unwrap();
    expiring["expires_at"] = json!(TermsTime::new(start + 2 * HOUR_MS).as_str());
    let expiring = AllowanceTerms::from_json(&expiring).unwrap();
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), expiring)).await;
    w.rail.set_history_payer(payer.identity());
    w.clock.set_ms(start + 3 * HOUR_MS);
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::AllowanceEnded
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_scope_missing_names_the_execute_scope() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    let mut phone = Box::pin(w.join(
        &w.alice,
        "phone",
        AppScopes::new().pay().allowance().build().unwrap(),
    ))
    .await;
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    Box::pin(phone.receive(None)).await.unwrap();
    assert_eq!(
        refused(Box::pin(phone.pay_under_allowance(&request, &key)).await),
        Refusal::ScopeMissing("paykit_allowance_execute".into())
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_another_app_executes_it() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    let mut phone = Box::pin(w.join(
        &w.alice,
        "phone",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    Box::pin(phone.receive(None)).await.unwrap();
    assert_eq!(
        refused(Box::pin(phone.pay_under_allowance(&request, &key)).await),
        Refusal::Usage("another app executes this Allowance".into())
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_request_of_another_counterparty_is_usage() {
    let (w, mut payer, _shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    let mut carol = Box::pin(w.join(
        &w.carol,
        "shop",
        AppScopes::new().request().build().unwrap(),
    ))
    .await;
    payer.app_mut().invite(&carol.identity()).await.unwrap();
    let request = Box::pin(one_time(&w, &mut carol, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::Usage("the request's payee isn't this Allowance's counterparty".into())
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_request_outside_its_terms_is_not_automatic() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    let endpoints = [(
        endpoint("btc-lightning-bolt11"),
        PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
    )]
    .into_iter()
    .collect();
    let in_usd = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("usd").unwrap(),
            value: Decimal::new("1").unwrap(),
        },
        PaymentReference::new("order-usd").unwrap(),
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(w.clock.now_ms() + 24 * HOUR_MS);
    let request = Box::pin(request_from(&mut shop, w.alice_id(), in_usd)).await;
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::NotAutomatic("the request does not qualify under this Allowance".into())
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_blocked_counterparty_pauses_until_unblocked() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_as(
        store(),
        terms(),
        // The wallet's own scopes: reading the block floor takes `paykit_pay`.
        AppScopes::new()
            .pay()
            .allowance_execute()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await;
    w.rail.set_history_payer(payer.identity());
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    // Blocking pauses what is open with the counterparty and revokes nothing (Paykit spec
    // 8.1): for now, not for good.
    Box::pin(payer.block(&shop.identity())).await.unwrap();
    assert_eq!(payer.allowance(&key).await.unwrap().blocked, Some(true));
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        not_now("the counterparty is blocked")
    );
    assert!(w.rail.all_executions().is_empty());
    // Unblocked, the counterparty's next request is paid. One dated before the block stays
    // below its block floor, which only the user's own payment passes (Paykit spec 8.1).
    Box::pin(payer.unblock(&shop.identity())).await.unwrap();
    // Past the block floor and its clock tolerance, the unblock's floor raise read
    // (Paykit spec 8.1).
    w.clock.advance_ms(11 * 60_000);
    Box::pin(payer.receive(None)).await.unwrap();
    Box::pin(pays(&w, &mut shop, &mut payer, &key)).await;
}
#[tokio::test]
async fn test_allowance_refusals_occurrence_paid_by_hand_is_manual_only() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    let mut phone =
        Box::pin(w.join(&w.alice, "phone", AppScopes::new().pay().build().unwrap())).await;
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    Box::pin(phone.receive(None)).await.unwrap();
    // The user tries it by hand from the phone, and the method fails it: the request is
    // marked manual-only.
    w.rail.next(Behaviour::Fail);
    let by_hand = Box::pin(phone.pay_request(&request, PayOptions::default()))
        .await
        .outcome
        .unwrap();
    assert!(matches!(by_hand, PayOutcome::Failed { .. }), "{by_hand:?}");
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::ManualOnly
    );
    // The mark is the occurrence's own, and is never lifted: it comes before the
    // Allowance's state, ended or not.
    Box::pin(payer.end_allowance(&key)).await.unwrap();
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::ManualOnly
    );
}

#[tokio::test]
async fn test_allowance_refusals_unanswered_proposal_is_not_now() {
    let (w, mut payer, mut shop, _key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    let second = Box::pin(payer.propose_executable_allowance(
        shop.identity(),
        terms(),
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &second.record)).await),
        not_now("the Allowance isn't accepted yet")
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_terms_not_active_yet_is_not_now() {
    let start = pubky_switch_client::testing::START_MS;
    let later = terms_with(json!({ "active_from": TermsTime::new(start + 2 * HOUR_MS).as_str() }));
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), later)).await;
    w.rail.set_history_payer(payer.identity());
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        not_now("the Allowance's terms aren't active yet")
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_payment_outside_the_per_payment_range_is_not_automatic() {
    let ranged = terms_with(json!({ "per_payment_amount": { "minimum": "1", "maximum": "2" } }));
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), ranged)).await;
    w.rail.set_history_payer(payer.identity());
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "5")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::NotAutomatic("the per_payment limit has no room".into())
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_lifetime_limit_waits_while_reserved_then_is_final() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    // 70 of 100 in flight: its reservation holds the room.
    let first = Box::pin(one_time(&w, &mut shop, &mut payer, "70")).await;
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = Box::pin(payer.pay_under_allowance(&first, &key))
        .await
        .outcome
        .unwrap()
    else {
        panic!("the first payment did not stay in flight");
    };
    let second = Box::pin(one_time(&w, &mut shop, &mut payer, "50")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&second, &key)).await),
        not_now("the lifetime limit has no room")
    );
    // Once the 70 are paid, nothing is reserved, and no room comes back.
    w.rail.settle(&attempt_id, true);
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&second, &key)).await),
        Refusal::NotAutomatic("the lifetime limit has no room".into())
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}
#[tokio::test]
async fn test_allowance_refusals_confirmation_needed_after_restore_until_confirmed() {
    let (w, mut payer, _shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    let backup = payer.backup().await.unwrap();
    drop(payer);
    // A restore that isn't clean: every Allowance waits for the user's confirmation again.
    let mut payer = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    crate::common::restore_with_unknown_cursor(&mut payer).await;
    Box::pin(payer.reconcile()).await.unwrap();
    Box::pin(payer.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert!(!view.consented && view.confirmation_needed, "{view:?}");
    for _ in 0..2 {
        assert_eq!(
            refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
            Refusal::AllowanceConfirmationNeeded { record: key }
        );
    }

    // The user confirms it: it is consented again, and asks for nothing more.
    Box::pin(payer.confirm_allowance_executor(&key, UserConfirmed::new()))
        .await
        .unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.consented && !view.confirmation_needed, "{view:?}");
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
}

/// The view's `confirmation_needed` is true exactly when a payment under the Allowance at
/// `key` is refused for the user's confirmation: the payment is made here to compare.
async fn view_agrees_with_payment(
    w: &World,
    shop: &mut Paykit,
    payer: &mut Paykit,
    key: &[u8; 32],
) -> (bool, Result<PayOutcome, Refusal>) {
    let shown = payer.allowance(key).await.unwrap().confirmation_needed;
    let request = Box::pin(one_time(w, shop, payer, "1")).await;
    // After a `missed` item the request is held too, as any open one: the user sees it may
    // have been cancelled, and the host confirms that first.
    if let Some(held) = payer.request(&request).await.unwrap().held {
        Box::pin(payer.confirm_possibly_cancelled(&request, &held, UserConfirmed::new()))
            .await
            .unwrap();
    }
    let paid = Box::pin(payer.pay_under_allowance(&request, key))
        .await
        .outcome
        .map_err(|error| error.refusal().cloned().unwrap());
    assert_eq!(
        shown,
        matches!(paid, Err(Refusal::AllowanceConfirmationNeeded { record }) if &record == key),
        "{paid:?}"
    );
    (shown, paid)
}

#[tokio::test]
async fn test_allowance_refusals_view_flag_matches_the_payment_refusal() {
    // Held and confirmed: no flag, and the payment goes.
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    let (shown, paid) = Box::pin(view_agrees_with_payment(&w, &mut shop, &mut payer, &key)).await;
    assert!(
        !shown && matches!(paid, Ok(PayOutcome::Paid { .. })),
        "{paid:?}"
    );

    // After a `missed` item every Allowance on the list waits for the user: the flag, and
    // the refusal naming it.
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    Box::pin(crate::common::finish_listing(&mut payer)).await;
    let (shown, _) = Box::pin(view_agrees_with_payment(&w, &mut shop, &mut payer, &key)).await;
    assert!(shown);
    Box::pin(payer.confirm_allowance_executor(&key, UserConfirmed::new()))
        .await
        .unwrap();
    let (shown, paid) = Box::pin(view_agrees_with_payment(&w, &mut shop, &mut payer, &key)).await;
    assert!(
        !shown && matches!(paid, Ok(PayOutcome::Paid { .. })),
        "{paid:?}"
    );

    // Accepted, but never taken up by this app: no flag; the payment waits.
    let second = Box::pin(payer.propose_executable_allowance(
        shop.identity(),
        terms(),
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let offered = shop
        .allowances()
        .await
        .unwrap()
        .into_iter()
        .find(|view| view.allowance_id == second.allowance_id)
        .unwrap();
    Box::pin(shop.accept_allowance(&offered.record))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let (shown, paid) = Box::pin(view_agrees_with_payment(
        &w,
        &mut shop,
        &mut payer,
        &second.record,
    ))
    .await;
    assert!(
        !shown && matches!(paid, Err(Refusal::NotNow(_))),
        "{paid:?}"
    );

    // Ended: no flag; the refusal is final.
    Box::pin(payer.end_allowance(&key)).await.unwrap();
    let (shown, paid) = Box::pin(view_agrees_with_payment(&w, &mut shop, &mut payer, &key)).await;
    assert!(!shown && paid == Err(Refusal::AllowanceEnded), "{paid:?}");
}

#[tokio::test]
async fn test_allowance_refusals_view_flag_is_off_for_a_conflicted_one() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    let id = payer.allowance(&key).await.unwrap().allowance_id;
    let mut other = terms().to_json().unwrap();
    other["lifetime_amount_limit"] = json!("0.20");
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[payer.app_id()],
        "paykit.allowance_proposal",
        Vec::new(),
        json!({ "allowance_id": id.as_str(), "proposer_role": "allowee", "allower_generation": 1, "terms": other })
            .as_object()
            .cloned()
            .unwrap(),
    );
    Box::pin(payer.receive(None)).await.unwrap();
    let (shown, paid) = Box::pin(view_agrees_with_payment(&w, &mut shop, &mut payer, &key)).await;
    assert!(
        !shown && paid == Err(Refusal::AllowanceConflicted),
        "{paid:?}"
    );
}

/// Terms that expire two hours after the test clock's start.
fn expiring() -> AllowanceTerms {
    let start = pubky_switch_client::testing::START_MS;
    terms_with(json!({ "expires_at": TermsTime::new(start + 2 * HOUR_MS).as_str() }))
}

#[tokio::test]
async fn test_allowance_refusals_expired_before_any_list_request_is_sent() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), expiring())).await;
    w.rail.set_history_payer(payer.identity());
    // A request to whatever list bob's apps share: the wallet holds none yet.
    let to_any = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new("btc").unwrap(),
            value: Decimal::new("1").unwrap(),
        },
        PaymentReference::new("order-any").unwrap(),
        RequestDestination::Any,
    )
    .with_proposal_expires_at_ms(w.clock.now_ms() + 24 * HOUR_MS);
    let request = Box::pin(request_from(&mut shop, w.alice_id(), to_any)).await;
    Box::pin(payer.receive(None)).await.unwrap();
    w.clock
        .set_ms(pubky_switch_client::testing::START_MS + 3 * HOUR_MS);
    Box::pin(payer.receive(None)).await.unwrap();
    let sent_before = payer
        .app()
        .sent_to(&shop.identity(), 0, u64::MAX)
        .await
        .unwrap()
        .len();
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::AllowanceEnded
    );
    let sent = payer
        .app()
        .sent_to(&shop.identity(), 0, u64::MAX)
        .await
        .unwrap();
    assert_eq!(sent.len(), sent_before, "{sent:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_expired_subscription_and_period_end_it() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), expiring())).await;
    w.rail.set_history_payer(payer.identity());
    let (request, _) = Box::pin(subscribed(&w, &mut shop, &mut payer, &key)).await;
    // Past the terms' expiry, inside the third period, it is refused as ended, before
    // anything asks for its destination.
    w.clock
        .set_ms(pubky_switch_client::testing::START_MS + 5 * HOUR_MS / 2);
    Box::pin(payer.receive(None)).await.unwrap();
    let later = PeriodRef {
        starts_at: pubky_switch_client::testing::START_MS + 2 * HOUR_MS,
        ends_at: pubky_switch_client::testing::START_MS + 3 * HOUR_MS,
    };
    assert_eq!(
        refused(Box::pin(payer.pay_period_under_allowance(&request, &later)).await),
        Refusal::AllowanceEnded
    );
    // And a new subscription can't be accepted under it.
    let start = w.clock.now_ms() / 1_000 * 1_000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let second = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("1", start).with_recurrence(schedule),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(
        refusal_of(Box::pin(payer.accept_subscription_under_allowance(&second, &key)).await),
        Refusal::AllowanceEnded
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_the_allowee_resuming_execution_is_usage() {
    let (w, _payer, shop, _key) = Box::pin(first_grant_in(store(), terms())).await;
    // A second app of bob's that holds the execute scope: bob is the allowee.
    let mut till = Box::pin(
        w.join(
            &w.bob,
            "till",
            AppScopes::new()
                .allowance()
                .allowance_execute()
                .request()
                .build()
                .unwrap(),
        ),
    )
    .await;
    let record = shop_record(&shop).await;
    Box::pin(crate::common::finish_listing(&mut till)).await;
    assert_eq!(
        refusal_of(Box::pin(till.resume_allowance_executor(&record)).await),
        Refusal::Usage("this identity doesn't pay under this Allowance".into())
    );
}

#[tokio::test]
async fn test_allowance_refusals_one_time_conversion_is_not_automatic() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    let rate =
        paykit_lib::Rate::new(Asset::new("btc").unwrap(), Decimal::new("1").unwrap()).unwrap();
    let priced = draft("1", w.clock.now_ms())
        .with_conversion(paykit_lib::bodies::Conversion::Fixed(vec![rate]));
    let request = Box::pin(request_from(&mut shop, w.alice_id(), priced)).await;
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::NotAutomatic("the request does not qualify under this Allowance".into())
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_endpoint_outside_its_terms_leaves_no_destination() {
    let narrow =
        terms_with(json!({ "allowed_payment_endpoint_identifiers": ["btc-lightning-bolt12"] }));
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), narrow)).await;
    w.rail.set_history_payer(payer.identity());
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    // The request's endpoints are taken only where the Allowance allows them: none is left.
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::NoDestination("Paykit spec 8.1: the request has no such endpoint".into())
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_confirmation_waits_while_blocked() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_as(
        store(),
        terms(),
        // The wallet's own scopes: reading the block floor takes `paykit_pay`.
        AppScopes::new()
            .pay()
            .allowance_execute()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await;
    w.rail.set_history_payer(payer.identity());
    Box::pin(missed(&w, &mut payer)).await;
    Box::pin(payer.block(&shop.identity())).await.unwrap();
    // Confirming can't lift a block: the user isn't asked yet.
    assert!(!payer.allowance(&key).await.unwrap().confirmation_needed);
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        not_now("the counterparty is blocked")
    );
    // Unblocked, the user is asked, confirms, and the counterparty's next request is paid.
    Box::pin(payer.unblock(&shop.identity())).await.unwrap();
    // Past the block floor and its clock tolerance, the unblock's floor raise read
    // (Paykit spec 8.1).
    w.clock.advance_ms(11 * 60_000);
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(payer.allowance(&key).await.unwrap().confirmation_needed);
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    Box::pin(confirm_hold(&mut payer, &request)).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::AllowanceConfirmationNeeded { record: key }
    );
    Box::pin(payer.confirm_allowance_executor(&key, UserConfirmed::new()))
        .await
        .unwrap();
    let paid = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_allowance_refusals_expired_is_not_asked_to_confirm() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), expiring())).await;
    w.rail.set_history_payer(payer.identity());
    Box::pin(missed(&w, &mut payer)).await;
    w.clock
        .set_ms(pubky_switch_client::testing::START_MS + 3 * HOUR_MS);
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(!payer.allowance(&key).await.unwrap().confirmation_needed);
    assert_eq!(
        refusal_of(Box::pin(payer.confirm_allowance_executor(&key, UserConfirmed::new())).await),
        Refusal::AllowanceEnded
    );
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    Box::pin(confirm_hold(&mut payer, &request)).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::AllowanceEnded
    );
}

#[tokio::test]
async fn test_allowance_refusals_rejected_by_the_seller() {
    // Alice proposes; bob rejects: nothing is done under it, for good.
    let w = World::new();
    let mut payer = Box::pin(w.join(
        &w.alice,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let mut shop = Box::pin(w.join(
        &w.bob,
        "shop",
        AppScopes::new().allowance().request().build().unwrap(),
    ))
    .await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    shop.app_mut().invite(&payer.identity()).await.unwrap();
    let proposal = Box::pin(payer.propose_executable_allowance(
        shop.identity(),
        terms(),
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let offered = shop_record(&shop).await;
    Box::pin(shop.reject_allowance(&offered)).await.unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    // Its records are removed on both sides: nothing is left to show or execute.
    let shown = payer.allowance(&proposal.record).await;
    assert_eq!(
        shown.err().and_then(|error| error.refusal().cloned()),
        Some(Refusal::NotFound)
    );
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&proposal.record)).await),
        Refusal::NotFound
    );
}

#[tokio::test]
async fn test_allowance_refusals_restore_after_a_payment_is_confirmed() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    // A payment moves the ledger past its original version before the backup.
    Box::pin(pays(&w, &mut shop, &mut payer, &key)).await;
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut payer = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    crate::common::restore_with_unknown_cursor(&mut payer).await;
    Box::pin(payer.reconcile()).await.unwrap();
    Box::pin(payer.end_reconcile(UserConfirmed::new()))
        .await
        .unwrap();
    assert!(payer.allowance(&key).await.unwrap().confirmation_needed);
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        Refusal::AllowanceConfirmationNeeded { record: key }
    );
    // The payment method's history accounts for the payment made before the backup, so
    // the user's confirmation is taken against the restored ledger.
    Box::pin(payer.confirm_allowance_executor(&key, UserConfirmed::new()))
        .await
        .unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.consented && !view.confirmation_needed, "{view:?}");
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    assert_eq!(w.rail.all_executions().len(), 1);
}
