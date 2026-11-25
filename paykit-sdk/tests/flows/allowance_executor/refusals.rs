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
use pubky_dispatch_client::app::UserConfirmed;
use pubky_dispatch_client::clock::Clock;
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

/// A `missed` item at alice's Dispatch, read: every Allowance on the wallet's consent list
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
    let start = pubky_dispatch_client::testing::START_MS;
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
        paykit_sdk::Refusal::HeldForUser(paykit_sdk::HeldForUserReason::CounterpartyBlocked(
            "the counterparty is blocked"
        ))
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
        paykit_sdk::Refusal::HeldForUser(paykit_sdk::HeldForUserReason::AllowanceExecutionHeld(
            "the Allowance isn't accepted yet"
        ))
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_terms_not_active_yet_is_not_now() {
    let start = pubky_dispatch_client::testing::START_MS;
    let later = terms_with(json!({ "active_from": TermsTime::new(start + 2 * HOUR_MS).as_str() }));
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), later)).await;
    w.rail.set_history_payer(payer.identity());
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        paykit_sdk::Refusal::NotNow(paykit_sdk::NotNowReason::CallAgain(
            "the Allowance's terms aren't active yet"
        ))
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
async fn test_allowance_refusals_lifetime_limit_counts_a_dispatched_payment_as_spent() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    // 70 of 100 handed off and still pending: spent whether it pays or fails (Paykit spec
    // 8.3), so 50 never fits.
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
        Refusal::NotAutomatic("the lifetime limit has no room".into())
    );
    // More than the lifetime limit alone never fits, whatever the reservation does.
    let over = Box::pin(one_time(&w, &mut shop, &mut payer, "101")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&over, &key)).await),
        Refusal::NotAutomatic("the lifetime limit has no room".into())
    );
    // Once the 70 are paid, nothing is reserved, and no room comes back.
    w.rail.settle(&attempt_id, true);
    Box::pin(payer.resume_payments()).await.unwrap();
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&second, &key)).await),
        Refusal::NotAutomatic("the lifetime limit has no room".into())
    );
    // 70 paid and 10 more handed off: 25 and 40 never fit, whatever the 10 answer.
    let third = Box::pin(one_time(&w, &mut shop, &mut payer, "10")).await;
    w.rail.next(Behaviour::Hang);
    assert!(matches!(
        Box::pin(payer.pay_under_allowance(&third, &key))
            .await
            .outcome
            .unwrap(),
        PayOutcome::Pending { .. }
    ));
    let fits_later = Box::pin(one_time(&w, &mut shop, &mut payer, "25")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&fits_later, &key)).await),
        Refusal::NotAutomatic("the lifetime limit has no room".into())
    );
    let never = Box::pin(one_time(&w, &mut shop, &mut payer, "40")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&never, &key)).await),
        Refusal::NotAutomatic("the lifetime limit has no room".into())
    );
    assert_eq!(w.rail.all_executions().len(), 2);
}
#[tokio::test]
async fn test_allowance_refusals_lifetime_limit_stays_final_once_the_method_reports_failure() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    // 70 of 100 at the method and still pending: 50 never fits, whatever the 70 answer.
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
    let never = Refusal::NotAutomatic("the lifetime limit has no room".into());
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&second, &key)).await),
        never
    );
    // The method reports the failure, and its outcome isn't written yet: the attempt is
    // still counted reserved, but it reached the method, so nothing gives its room back.
    w.rail.settle(&attempt_id, false);
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&second, &key)).await),
        never
    );
    // The failure written: an attempt that reached the method stays counted (Paykit spec
    // 8.3), so the same payment is refused the same way.
    let resumed = Box::pin(payer.resume_payments()).await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Failed { .. }]),
        "{resumed:?}"
    );
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&second, &key)).await),
        never
    );
    // What still fits beside the 70 goes.
    let fits = Box::pin(one_time(&w, &mut shop, &mut payer, "30")).await;
    assert!(matches!(
        Box::pin(payer.pay_under_allowance(&fits, &key))
            .await
            .outcome
            .unwrap(),
        PayOutcome::Paid { .. }
    ));
    assert_eq!(w.rail.all_executions().len(), 2);
}

#[tokio::test]
async fn test_allowance_refusals_lifetime_limit_waits_for_a_reservation_before_the_call() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    w.rail.set_history_payer(payer.identity());
    // 70 of 100 made on the method, its history showing it paid while the claim is still
    // open and the journal still at the call: spent.
    let first = Box::pin(one_time(&w, &mut shop, &mut payer, "70")).await;
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = Box::pin(payer.pay_under_allowance(&first, &key))
        .await
        .outcome
        .unwrap()
    else {
        panic!("the first payment did not stay in flight");
    };
    w.rail.settle(&attempt_id, true);
    // 10 more reserved, by an attempt whose answer was lost before the method was called.
    let second = Box::pin(one_time(&w, &mut shop, &mut payer, "10")).await;
    w.alice
        .switch()
        .fail_next(pubky_dispatch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let lost = Box::pin(payer.pay_under_allowance(&second, &key))
        .await
        .outcome
        .unwrap_err();
    assert!(lost.kept_batch().is_some(), "{lost:?}");
    Box::pin(payer.receive(None)).await.unwrap();
    assert_eq!(w.rail.all_executions().len(), 1);
    // 25 fits once those 10 are released, which their own outcome does: it waits. 40 never
    // fits, even then.
    let fits_later = Box::pin(one_time(&w, &mut shop, &mut payer, "25")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&fits_later, &key)).await),
        paykit_sdk::Refusal::NotNow(paykit_sdk::NotNowReason::AllowanceLimit {
            limit: "lifetime".into()
        })
    );
    let never = Box::pin(one_time(&w, &mut shop, &mut payer, "40")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&never, &key)).await),
        Refusal::NotAutomatic("the lifetime limit has no room".into())
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_allowance_refusals_a_count_limit_of_none_is_final() {
    // A period that admits no payment never makes room: final for automatic payment, even
    // in an empty window.
    let terms = terms_with(json!({
        "period_limits": [{
            "amount_limit": null,
            "payment_count_limit": 0,
            "period": {"kind": "rolling", "every": 1, "unit": "day"},
        }],
    }));
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms)).await;
    w.rail.set_history_payer(payer.identity());
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::NotAutomatic("the period_count limit has no room".into())
    );
    assert!(w.rail.all_executions().is_empty());
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
            Refusal::AllowanceConfirmationNeeded {
                hold: payer.allowance(&key).await.unwrap().hold
            }
        );
    }

    // The user confirms it: it is consented again, and asks for nothing more.
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
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
        matches!(&paid, Err(Refusal::AllowanceConfirmationNeeded { hold }) if hold.record() == *key),
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
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    let (shown, paid) = Box::pin(view_agrees_with_payment(&w, &mut shop, &mut payer, &key)).await;
    assert!(
        !shown && matches!(paid, Ok(PayOutcome::Paid { .. })),
        "{paid:?}"
    );

    // Accepted, but never taken up by this app: no flag; the payment is held for the host.
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
        !shown
            && matches!(
                paid,
                Err(Refusal::HeldForUser(
                    paykit_sdk::HeldForUserReason::AllowanceExecutionHeld(_)
                ))
            ),
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
    let start = pubky_dispatch_client::testing::START_MS;
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
        .set_ms(pubky_dispatch_client::testing::START_MS + 3 * HOUR_MS);
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
        .set_ms(pubky_dispatch_client::testing::START_MS + 5 * HOUR_MS / 2);
    Box::pin(payer.receive(None)).await.unwrap();
    let later = PeriodRef {
        starts_at: pubky_dispatch_client::testing::START_MS + 2 * HOUR_MS,
        ends_at: pubky_dispatch_client::testing::START_MS + 3 * HOUR_MS,
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
        paykit_sdk::Refusal::HeldForUser(paykit_sdk::HeldForUserReason::CounterpartyBlocked(
            "the counterparty is blocked"
        ))
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
        Refusal::AllowanceConfirmationNeeded {
            hold: payer.allowance(&key).await.unwrap().hold
        }
    );
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
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
        .set_ms(pubky_dispatch_client::testing::START_MS + 3 * HOUR_MS);
    Box::pin(payer.receive(None)).await.unwrap();
    assert!(!payer.allowance(&key).await.unwrap().confirmation_needed);
    assert_eq!(
        refusal_of(
            Box::pin(payer.confirm_allowance_executor(
                &payer.allowance(&key).await.unwrap().hold,
                UserConfirmed::new()
            ))
            .await
        ),
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
        Refusal::AllowanceConfirmationNeeded {
            hold: payer.allowance(&key).await.unwrap().hold
        }
    );
    // The payment method's history accounts for the payment made before the backup, so
    // the user's confirmation is taken against the restored ledger.
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.consented && !view.confirmation_needed, "{view:?}");
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    assert_eq!(w.rail.all_executions().len(), 1);
}

// What the grant paths answer while the first grant and its consent land (Paykit spec 7.2,
// 8.3): the grant's write kept to send again waits for it, a grant applied under an older
// hold asks the user, a removal kept before the consent is final, and a `missed` item's
// mark waits with its own reason.

#[tokio::test]
async fn test_allowance_refusals_kept_grant_write_is_not_now_until_it_lands() {
    use pubky_dispatch_client::testing::Fault;
    let (w, mut payer, mut shop, key) = Box::pin(super::accepted_not_granted(store())).await;
    let unreachable = || Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    };
    w.alice.switch().fail_next(unreachable());
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's batch is kept");
    // While the batch can't be sent again, the resume and a payment under the Allowance
    // wait for it, and the user is asked nothing.
    let kept = paykit_sdk::Refusal::NotNow(paykit_sdk::NotNowReason::EarlierWriteUnresolved(
        "the executor grant's write waits to be sent again",
    ));
    w.alice.switch().fail_next(unreachable());
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        kept
    );
    assert!(!payer.allowance(&key).await.unwrap().confirmation_needed);
    w.alice.switch().fail_next(unreachable());
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    // Refused before anything is sent.
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        kept
    );

    // Once it can be sent, the next resume lands it and consents.
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.consented && !view.confirmation_needed, "{view:?}");
}

#[tokio::test]
async fn test_allowance_refusals_grant_applied_under_an_older_hold_asks_the_user() {
    // The first grant's batch is kept and lands after a `missed` item: the approval it was
    // made on is the earlier hold's, so the user confirms it, and confirming lifts it.
    use pubky_dispatch_client::testing::Fault;
    let (w, mut payer, _shop, key) = Box::pin(super::accepted_not_granted(store())).await;
    w.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's batch is unanswered");
    w.clock.advance_ms(1_000);
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    for _ in 0..3 {
        Box::pin(payer.receive(None)).await.unwrap();
    }
    let view = payer.allowance(&key).await.unwrap();
    assert_eq!(view.executor_app_id, Some(payer.app_id()), "{view:?}");
    assert!(!view.consented && view.confirmation_needed, "{view:?}");
    for _ in 0..2 {
        assert_eq!(
            refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
            Refusal::AllowanceConfirmationNeeded {
                hold: payer.allowance(&key).await.unwrap().hold
            }
        );
    }
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.consented && !view.confirmation_needed, "{view:?}");
}

#[tokio::test]
async fn test_allowance_refusals_removal_before_the_consent_is_final() {
    // Another own app ends the Allowance once the kept grant applied, before its consent is
    // written: the removal is kept, and the grant paths refuse for good, asking nothing.
    use pubky_dispatch_client::testing::Fault;
    let (w, mut payer, _shop, key) = Box::pin(super::accepted_not_granted(store())).await;
    let _phone = Box::pin(w.join(
        &w.alice,
        "phone",
        AppScopes::new().allowance().build().unwrap(),
    ))
    .await;
    w.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's batch is unanswered");
    Box::pin(payer.receive(None)).await.unwrap();
    crate::allowances::notice(&w, &w.alice, "phone", payer.app_id(), &key, "ended");
    Box::pin(payer.receive(None)).await.unwrap();
    for _ in 0..2 {
        assert_eq!(
            refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
            Refusal::AllowanceEnded
        );
        assert_eq!(
            refusal_of(
                Box::pin(payer.confirm_allowance_executor(
                    &payer.allowance(&key).await.unwrap().hold,
                    UserConfirmed::new()
                ))
                .await
            ),
            Refusal::AllowanceEnded
        );
    }
    let view = payer.allowance(&key).await.unwrap();
    assert!(!view.consented && !view.confirmation_needed, "{view:?}");
}

#[tokio::test]
async fn test_allowance_refusals_missed_mark_waits_with_its_reason() {
    // A `missed` item's mark is kept and the entries aren't marked yet, as after a crash
    // between the two commits: nothing is consented and the user is asked nothing until the
    // next receive marks them; then the user confirms.
    use pubky_dispatch_client::app::LocalWrite;
    let (_w, mut payer, _shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    payer
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: b"mc".to_vec(),
                value: br#"{"v":1,"d":true}"#.to_vec(),
            }],
        )
        .await
        .unwrap();
    // Only the next receive marks it: a wait.
    let waits = paykit_sdk::Refusal::NotNow(paykit_sdk::NotNowReason::ReadAgain(
        "a missed item holds every Allowance until the next receive marks it for confirmation",
    ));
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        waits
    );
    assert_eq!(
        refusal_of(
            Box::pin(payer.confirm_allowance_executor(
                &payer.allowance(&key).await.unwrap().hold,
                UserConfirmed::new()
            ))
            .await
        ),
        waits
    );
    let view = payer.allowance(&key).await.unwrap();
    assert!(!view.consented && !view.confirmation_needed, "{view:?}");

    Box::pin(payer.receive(None)).await.unwrap();
    assert!(payer.allowance(&key).await.unwrap().confirmation_needed);
    Box::pin(payer.confirm_allowance_executor(
        &payer.allowance(&key).await.unwrap().hold,
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    assert!(payer.allowance(&key).await.unwrap().consented);
}

// A confirmation counts for the hold it was given under (Paykit spec 7.2, 8.3).

#[tokio::test]
async fn test_allowance_refusals_approval_lost_before_the_grant_asks_the_user() {
    // The user approved automatic execution and a `missed` item took the approval before
    // the first grant: the user is asked to confirm again, everywhere, and confirming grants.
    let (w, mut payer, mut shop, key) = Box::pin(super::accepted_not_granted(store())).await;
    Box::pin(missed(&w, &mut payer)).await;
    let view = payer.allowance(&key).await.unwrap();
    assert!(
        view.confirmation_needed && !view.consented && view.executor_app_id.is_none(),
        "{view:?}"
    );
    let needed = Refusal::AllowanceConfirmationNeeded {
        hold: view.hold.clone(),
    };
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        needed
    );
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    Box::pin(confirm_hold(&mut payer, &request)).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        needed
    );
    Box::pin(payer.confirm_allowance_executor(&view.hold, UserConfirmed::new()))
        .await
        .unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.consented && !view.confirmation_needed, "{view:?}");
    assert_eq!(view.executor_app_id, Some(payer.app_id()));
}

#[tokio::test]
async fn test_allowance_refusals_confirmation_for_an_older_hold_asks_again() {
    // The user confirms what a first `missed` item held, after a second one: the
    // confirmation is refused with the hold in force now, nothing is written, and the
    // confirmation the user gives for that one consents.
    let (w, mut payer, _shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    Box::pin(missed(&w, &mut payer)).await;
    let first = payer.allowance(&key).await.unwrap();
    assert!(first.confirmation_needed, "{first:?}");
    w.clock.advance_ms(1_000);
    Box::pin(missed(&w, &mut payer)).await;
    let second = payer.allowance(&key).await.unwrap().hold;
    assert_ne!(first.hold, second);
    assert_eq!(
        refusal_of(
            Box::pin(payer.confirm_allowance_executor(&first.hold, UserConfirmed::new())).await
        ),
        Refusal::AllowanceConfirmationNeeded {
            hold: second.clone()
        }
    );
    assert!(!payer.allowance(&key).await.unwrap().consented);
    Box::pin(payer.confirm_allowance_executor(&second, UserConfirmed::new()))
        .await
        .unwrap();
    assert!(payer.allowance(&key).await.unwrap().consented);
}

/// A store that, once armed, sets a new hold (`he`) in the commit that lands a kept first
/// grant, as a restore verdict or a `missed` item read with it would.
#[derive(Default)]
struct HoldOnLanding {
    inner: pubky_dispatch_client::store::MemoryStore,
    armed: std::sync::atomic::AtomicBool,
}

impl HoldOnLanding {
    fn with_hold(
        &self,
        batch: pubky_dispatch_client::store::WriteBatch,
    ) -> pubky_dispatch_client::store::WriteBatch {
        use pubky_dispatch_client::store::{Write, WriteBatch};
        let landing = batch.writes().iter().find_map(|write| match write {
            Write::Delete { ns, key } if key.starts_with(b"p/paykit/gb/") => Some(ns.clone()),
            _ => None,
        });
        let Some(ns) =
            landing.filter(|_| self.armed.swap(false, std::sync::atomic::Ordering::SeqCst))
        else {
            return batch;
        };
        let mut out = WriteBatch::new();
        for write in batch.into_writes() {
            match write {
                Write::Put { ns, key, value } => out.put(&ns, key, value).unwrap(),
                Write::Delete { ns, key } => out.delete(&ns, key).unwrap(),
                Write::DeletePrefix { ns, prefix } => out.delete_prefix(&ns, prefix).unwrap(),
            };
        }
        let epoch = pubky_dispatch_core::enc::b64(&[0x5a; 16]);
        out.put(
            &ns,
            b"p/paykit/he".to_vec(),
            format!(r#"{{"v":1,"d":"{epoch}"}}"#),
        )
        .unwrap();
        out
    }
}

impl pubky_dispatch_client::store::StateStore for HoldOnLanding {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_dispatch_client::error::StoreError> {
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_dispatch_client::store::KeyValue>, pubky_dispatch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_dispatch_client::store::WriteBatch,
    ) -> Result<(), pubky_dispatch_client::error::StoreError> {
        self.inner.commit(self.with_hold(batch))
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_dispatch_client::store::StoreOwner,
    ) -> Result<
        pubky_dispatch_client::store::AcquireOutcome,
        pubky_dispatch_client::error::StoreError,
    > {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_dispatch_client::store::StoreOwner,
        batch: pubky_dispatch_client::store::WriteBatch,
    ) -> Result<pubky_dispatch_client::store::CommitOutcome, pubky_dispatch_client::error::StoreError>
    {
        self.inner.commit_if(ns, owner, self.with_hold(batch))
    }
}

#[tokio::test]
async fn test_allowance_refusals_hold_set_while_landing_the_grant_asks_again() {
    // The confirmed first grant's batch is kept; the read that lands it also sets a new
    // hold. The same call consents nothing: the hold it was confirmed for isn't the one in
    // force now, and it asks again with the new one.
    use pubky_dispatch_client::testing::Fault;
    let disk = Arc::new(HoldOnLanding::default());
    let (w, mut payer, _shop, key) =
        Box::pin(super::accepted_not_granted(Arc::clone(&disk) as Arc<_>)).await;
    let shown = payer.allowance(&key).await.unwrap().hold;
    w.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    Box::pin(payer.confirm_allowance_executor(&shown, UserConfirmed::new()))
        .await
        .expect_err("the grant's batch is unanswered");
    disk.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let refused =
        refusal_of(Box::pin(payer.confirm_allowance_executor(&shown, UserConfirmed::new())).await);
    assert!(
        matches!(&refused, Refusal::AllowanceConfirmationNeeded { hold } if *hold != shown),
        "{refused:?}"
    );
    assert!(
        !disk.armed.load(std::sync::atomic::Ordering::SeqCst),
        "the grant landed"
    );
    assert!(!payer.allowance(&key).await.unwrap().consented);
}

#[tokio::test]
async fn test_allowance_refusals_known_end_comes_before_a_kept_grant() {
    // The first grant's batch is kept while another own app's end is read: the grant paths
    // answer the end, as a payment does, not that the grant waits to be sent.
    use pubky_dispatch_client::testing::Fault;
    let (w, mut payer, _shop, key) = Box::pin(super::accepted_not_granted(store())).await;
    let _phone = Box::pin(w.join(
        &w.alice,
        "phone",
        AppScopes::new().allowance().build().unwrap(),
    ))
    .await;
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .expect_err("the grant's answer is lost");
    crate::allowances::notice(&w, &w.alice, "phone", payer.app_id(), &key, "ended");
    let unreachable = || Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    };
    w.alice.switch().fail_next(unreachable());
    Box::pin(payer.receive(None)).await.unwrap();
    let hold = payer.allowance(&key).await.unwrap().hold;
    for _ in 0..2 {
        w.alice.switch().fail_next(unreachable());
        assert_eq!(
            refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
            Refusal::AllowanceEnded
        );
        w.alice.switch().fail_next(unreachable());
        assert_eq!(
            refusal_of(
                Box::pin(payer.confirm_allowance_executor(&hold, UserConfirmed::new())).await
            ),
            Refusal::AllowanceEnded
        );
    }
}

#[tokio::test]
async fn test_allowance_refusals_grant_after_the_backup_asks_no_confirmation() {
    // The user approved automatic execution, the backup was taken, and then the grant
    // applied. Restored from that backup, the record shows this app's grant with no history
    // of it here: no confirmation lands it, so none is asked for (Paykit spec 7.2, 8.3).
    let (w, mut payer, _shop, key) = Box::pin(super::accepted_not_granted(store())).await;
    w.rail.set_history_payer(payer.identity());
    let backup = payer.backup().await.unwrap();
    Box::pin(payer.resume_allowance_executor(&key))
        .await
        .unwrap();
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
    let view = payer.allowance(&key).await.unwrap();
    assert_eq!(view.executor_app_id, Some(payer.app_id()), "{view:?}");
    assert!(!view.consented && !view.confirmation_needed, "{view:?}");
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        paykit_sdk::Refusal::HeldForUser(paykit_sdk::HeldForUserReason::AllowanceExecutionHeld(
            "this app's grant is held, but its consent isn't on this device"
        ))
    );
}

#[tokio::test]
async fn test_allowance_refusals_expiry_since_the_last_receive_ends_the_grant_paths() {
    // The terms expire after the wallet's last receive: the grant paths' fresh reads bring
    // the service time up to date, and they refuse the Allowance as ended, writing nothing.
    let (w, mut payer, _shop, key) = Box::pin(first_grant_in(store(), expiring())).await;
    let hold = payer.allowance(&key).await.unwrap().hold;
    w.clock
        .set_ms(pubky_dispatch_client::testing::START_MS + 3 * HOUR_MS);
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        Refusal::AllowanceEnded
    );
    assert_eq!(
        refusal_of(Box::pin(payer.confirm_allowance_executor(&hold, UserConfirmed::new())).await),
        Refusal::AllowanceEnded
    );
}

#[tokio::test]
async fn test_allowance_refusals_removal_comes_before_a_missed_mark() {
    // Another own app's end is recorded as a removal, then a `missed` item's mark is kept
    // before the entries are marked: the removal is final all the same.
    use pubky_dispatch_client::app::LocalWrite;
    let (w, mut payer, _shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    let _phone = Box::pin(w.join(
        &w.alice,
        "phone",
        AppScopes::new().allowance().build().unwrap(),
    ))
    .await;
    crate::allowances::notice(&w, &w.alice, "phone", payer.app_id(), &key, "ended");
    Box::pin(payer.receive(None)).await.unwrap();
    payer
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: b"mc".to_vec(),
                value: br#"{"v":1,"d":true}"#.to_vec(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        Refusal::AllowanceEnded
    );
}

#[tokio::test]
async fn test_allowance_refusals_restore_before_the_acceptance_asks_no_confirmation() {
    // The backup holds the user's approval from the proposal but not the acceptance that
    // opened the Allowance. Restored from it, the Allowance isn't asked to be confirmed:
    // confirming couldn't grant it without that opening.
    let w = World::new();
    let mut payer = Box::pin(w.join_with(
        &w.alice,
        "wallet",
        AppScopes::new().allowance_execute().build().unwrap(),
        PaykitConfig::default(),
        store(),
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
    w.rail.set_history_payer(payer.identity());
    Box::pin(payer.propose_executable_allowance(shop.identity(), terms(), UserConfirmed::new()))
        .await
        .unwrap();
    let backup = payer.backup().await.unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let offer = shop.allowances().await.unwrap().pop().unwrap();
    Box::pin(shop.accept_allowance(&offer.record))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let key = payer.allowances().await.unwrap().pop().unwrap().record;
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
    let view = payer.allowance(&key).await.unwrap();
    assert!(!view.confirmation_needed, "{view:?}");
    assert!(!matches!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        Refusal::AllowanceConfirmationNeeded { .. }
    ));
}

#[tokio::test]
async fn test_allowance_refusals_stale_subscription_copy_waits_for_a_read() {
    // Another wallet of alice's rejects the subscription after this wallet last read it.
    // Its answer under the Allowance starts from a copy older than the fresh record: a
    // wait, not a hold for the user (Paykit spec 8.1); the next answer, read again, sees
    // the rejection.
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    let start = w.clock.now_ms() / 1_000 * 1_000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let request = Box::pin(request_from(
        &mut shop,
        payer.identity(),
        draft("1", start).with_recurrence(schedule),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    let mut phone = w
        .join_with(
            &w.alice,
            "phone",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
            paykit_sdk::PaykitConfig::default(),
            store(),
        )
        .await;
    Box::pin(phone.receive(None)).await.unwrap();
    Box::pin(phone.reject_request(&request, None))
        .await
        .unwrap();
    let refused =
        refusal_of(Box::pin(payer.accept_subscription_under_allowance(&request, &key)).await);
    assert!(
        matches!(
            refused,
            Refusal::NotNow(paykit_sdk::NotNowReason::ReadAgain(_))
        ),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}
