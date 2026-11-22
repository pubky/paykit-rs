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
    // Bob's app sends a second, different proposal of the same Allowance.
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
    assert_eq!(
        payer.allowance(&key).await.unwrap().status,
        AllowanceStatus::Conflicted
    );
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert_eq!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
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
    assert!(matches!(
        refused(Box::pin(phone.pay_under_allowance(&request, &key)).await),
        Refusal::Usage(_)
    ));
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
    assert!(matches!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::Usage(_)
    ));
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
    let refusal = refused(Box::pin(payer.pay_under_allowance(&request, &key)).await);
    assert!(matches!(refusal, Refusal::NotAutomatic(_)), "{refusal:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_refusals_blocked_counterparty_is_not_automatic() {
    let (w, mut payer, mut shop, key) = Box::pin(first_grant_as(
        store(),
        terms(),
        AppScopes::new()
            .allowance_execute()
            .manage_contacts()
            .build()
            .unwrap(),
    ))
    .await;
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    Box::pin(payer.block(&shop.identity())).await.unwrap();
    assert_eq!(payer.allowance(&key).await.unwrap().blocked, Some(true));
    assert!(matches!(
        refused(Box::pin(payer.pay_under_allowance(&request, &key)).await),
        Refusal::NotAutomatic(_)
    ));
    assert!(w.rail.all_executions().is_empty());
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
}

#[tokio::test]
async fn test_allowance_refusals_unanswered_proposal_is_not_now() {
    let (w, mut payer, mut shop, _key) = Box::pin(first_grant_in(store(), terms())).await;
    let second = Box::pin(payer.propose_executable_allowance(
        shop.identity(),
        terms(),
        UserConfirmed::new(),
    ))
    .await
    .unwrap();
    let request = Box::pin(one_time(&w, &mut shop, &mut payer, "1")).await;
    assert!(matches!(
        refused(Box::pin(payer.pay_under_allowance(&request, &second.record)).await),
        Refusal::NotNow(_)
    ));
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
            Refusal::AllowanceConfirmationNeeded { record: key }
        );
    }

    // The user confirms it: it is consented again, and asks for nothing more.
    Box::pin(payer.confirm_allowance_executor(&key, UserConfirmed::new()))
        .await
        .unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.consented && !view.confirmation_needed, "{view:?}");
    assert!(w.rail.all_executions().is_empty());
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
