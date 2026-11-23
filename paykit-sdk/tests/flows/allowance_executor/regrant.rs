//! After a provider loss (Paykit spec 8.3, "After provider loss"): an Allowance this app
//! executes, last confirmed in a generation the loss ended, waits for the user's
//! confirmation again, bound to the hold it was shown under; the confirmation writes its
//! grant at the current generation, which establishes no complete usage authority, so
//! nothing is paid automatically until its usage is repaired.

use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};
use paykit_lib::records::AllowanceRecord;
use paykit_sdk::{AllowanceStatus, AppScopes, Paykit, Refusal};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;

use super::{first_grant_in, terms};
use crate::common::{draft, request_from, store, World};

const NEEDS_REPAIR: &str = "after a provider loss, the Allowance's usage record needs a repair before it pays automatically; the user can end it and set up a new one";

fn refusal_of<T: std::fmt::Debug>(result: paykit_sdk::Result<T>) -> Refusal {
    result
        .expect_err("refused")
        .refusal()
        .cloned()
        .expect("a refusal")
}

/// An Allowance granted, then a provider loss the payer has followed and reconciled.
async fn granted_then_lost() -> (World, Paykit, Paykit, [u8; 32]) {
    let (w, payer, shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    Box::pin(lost(w, payer, shop, key)).await
}

/// [`granted_then_lost`] from an Allowance already granted.
async fn lost(
    w: World,
    mut payer: Paykit,
    mut shop: Paykit,
    key: [u8; 32],
) -> (World, Paykit, Paykit, [u8; 32]) {
    w.rail.set_history_payer(payer.identity());
    let _service = crate::provider_loss::declare_loss(&w);
    let until = Box::pin(crate::provider_loss::follow_loss(&mut payer)).await;
    payer.app_mut().invite(&shop.identity()).await.unwrap();
    w.clock
        .set_ms(until.max(w.clock.now_ms()) + pubky_switch_client::app::DOC_CACHE_MS);
    Box::pin(shop.receive(None)).await.unwrap();
    (w, payer, shop, key)
}

/// The Allowance record at `key` as `payer` holds it.
async fn record(payer: &Paykit, key: &[u8; 32]) -> AllowanceRecord {
    let held = payer.app().shared_record(key).await.unwrap().unwrap();
    AllowanceRecord::read(held.content.value().unwrap(), &payer.identity()).unwrap()
}

/// The generation the user last confirmed the Allowance at `key` in, and its grant count.
async fn confirmed(payer: &Paykit, key: &[u8; 32]) -> (u64, usize) {
    let held = record(payer, key).await;
    let generation = held
        .current_grant(&payer.identity())
        .map(|(grant, _)| grant.confirmed_generation)
        .unwrap();
    (generation, held.grants.len())
}

/// A subscription from the shop, accepted under the Allowance at `key`: its refusal.
async fn subscription_refused(
    w: &World,
    shop: &mut Paykit,
    payer: &mut Paykit,
    key: &[u8; 32],
) -> Refusal {
    let start = w.clock.now_ms() / 1_000 * 1_000;
    let schedule = Recurrence::new(
        AnchoredRule::new(start, 1, Unit::Hour).unwrap(),
        start,
        None,
    )
    .unwrap();
    let request = Box::pin(request_from(
        shop,
        payer.identity(),
        draft("1", start).with_recurrence(schedule),
    ))
    .await;
    Box::pin(payer.receive(None)).await.unwrap();
    refusal_of(Box::pin(payer.accept_subscription_under_allowance(&request, key)).await)
}

#[tokio::test]
async fn test_allowance_regrant_after_a_provider_loss_writes_the_current_generation() {
    let (w, mut payer, mut shop, key) = Box::pin(granted_then_lost()).await;
    let lost = payer.app().own_identity().body().recovery_from.unwrap();
    let (before, grants) = Box::pin(confirmed(&payer, &key)).await;
    assert!(before <= lost);
    // Before the confirmation: the user is asked, at the view, at a resume and at a
    // subscription's acceptance, with the hold shown.
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.confirmation_needed, "{view:?}");
    let asked = Refusal::AllowanceConfirmationNeeded {
        hold: view.hold.clone(),
    };
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        asked
    );
    assert_eq!(
        Box::pin(subscription_refused(&w, &mut shop, &mut payer, &key)).await,
        asked
    );
    // The confirmation writes the grant at the current generation.
    Box::pin(payer.confirm_allowance_executor(&view.hold, UserConfirmed::new()))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let generation = payer.app().own_identity().generation();
    assert_eq!(
        Box::pin(confirmed(&payer, &key)).await,
        (generation, grants + 1)
    );
    // It establishes no usage authority: nothing is paid, and the user isn't asked again.
    let view = payer.allowance(&key).await.unwrap();
    assert!(!view.confirmation_needed, "{view:?}");
    let waits = Refusal::NotNow(NEEDS_REPAIR.into());
    assert_eq!(
        Box::pin(subscription_refused(&w, &mut shop, &mut payer, &key)).await,
        waits
    );
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        waits
    );
    // A second confirmation writes no second grant.
    assert_eq!(
        refusal_of(
            Box::pin(payer.confirm_allowance_executor(&view.hold, UserConfirmed::new())).await
        ),
        waits
    );
    assert_eq!(
        Box::pin(confirmed(&payer, &key)).await,
        (generation, grants + 1)
    );
    assert!(w.rail.all_executions().is_empty());
    // The user can always end it.
    Box::pin(payer.end_allowance(&key)).await.unwrap();
    assert_eq!(
        payer.allowance(&key).await.unwrap().status,
        AllowanceStatus::Ended
    );
}

#[tokio::test]
async fn test_allowance_regrant_one_time_requests_stay_held_after_the_regrant() {
    // The shared post-loss hold on one-time payments answers first, before and after the
    // regrant (Paykit spec 8.1, 8.3).
    let (w, mut payer, mut shop, key) = Box::pin(granted_then_lost()).await;
    let held_for_good = Refusal::NotAutomatic(
        "Paykit spec 8.1: automatic one-time payments are held for good after a provider loss"
            .into(),
    );
    let one_time = |w: &World| draft("1", w.clock.now_ms());
    let request = Box::pin(request_from(&mut shop, payer.identity(), one_time(&w))).await;
    Box::pin(payer.receive(None)).await.unwrap();
    let paid = Box::pin(payer.pay_under_allowance(&request, &key)).await;
    assert_eq!(paid.outcome.unwrap_err().refusal(), Some(&held_for_good));
    let hold = payer.allowance(&key).await.unwrap().hold;
    Box::pin(payer.confirm_allowance_executor(&hold, UserConfirmed::new()))
        .await
        .unwrap();
    let paid = Box::pin(payer.pay_under_allowance(&request, &key)).await;
    assert_eq!(paid.outcome.unwrap_err().refusal(), Some(&held_for_good));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_regrant_confirmation_before_a_second_loss_writes_nothing() {
    // The user was shown the Allowance after one loss; a second loss came before the
    // answer. That confirmation counts for nothing: it is refused with the hold in force
    // now, no grant is written, and the user is asked with the new hold.
    let (_w, mut payer, _shop, key) = Box::pin(granted_then_lost()).await;
    let w = &_w;
    let shown = payer.allowance(&key).await.unwrap().hold;
    let before = Box::pin(confirmed(&payer, &key)).await;
    let _service = crate::provider_loss::declare_second_loss(w);
    Box::pin(crate::provider_loss::follow_loss(&mut payer)).await;
    let Refusal::AllowanceConfirmationNeeded { hold } =
        refusal_of(Box::pin(payer.confirm_allowance_executor(&shown, UserConfirmed::new())).await)
    else {
        panic!("the older confirmation was not refused for the newer hold");
    };
    assert_ne!(hold, shown);
    assert_eq!(Box::pin(confirmed(&payer, &key)).await, before);
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.confirmation_needed, "{view:?}");
    assert_eq!(view.hold, hold);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_regrant_no_other_own_app_lifts_the_pause() {
    // Another app of the identity can't confirm for the executor, and a record it writes
    // with the executor's grant claiming the current generation doesn't verify: the
    // executor's pause stays (Paykit spec 8.3: no other app can end this generation pause).
    let (w, payer, shop, key) = Box::pin(first_grant_in(store(), terms())).await;
    let mut phone = Box::pin(w.join(
        &w.alice,
        "phone",
        AppScopes::new().allowance_execute().build().unwrap(),
    ))
    .await;
    let (w, mut payer, mut shop, key) = Box::pin(lost(w, payer, shop, key)).await;
    Box::pin(crate::provider_loss::follow_loss(&mut phone)).await;
    let hold = phone.allowance(&key).await.unwrap().hold;
    assert_eq!(
        refusal_of(Box::pin(phone.confirm_allowance_executor(&hold, UserConfirmed::new())).await),
        Refusal::Usage("another app executes this Allowance".into())
    );
    let mut forged = Box::pin(record(&phone, &key)).await;
    let generation = phone.app().own_identity().generation();
    if let Some(entry) = forged.grants.last_mut() {
        entry.grant.confirmed_generation = generation;
        forged.grant = Some(entry.grant.clone());
    }
    let view = payer.allowance(&key).await.unwrap();
    let target = crate::allowances::allowances_target(&view.counterparty, &view.allowance_id);
    let write = phone
        .app_mut()
        .record_op(&target, Some(&forged.to_json().unwrap()), vec![])
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::new().entry(write))
        .await
        .unwrap();
    Box::pin(payer.receive(None)).await.unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert!(view.confirmation_needed, "{view:?}");
    assert_eq!(
        Box::pin(subscription_refused(&w, &mut shop, &mut payer, &key)).await,
        Refusal::AllowanceConfirmationNeeded { hold: view.hold }
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_allowance_regrant_ending_while_paused_works() {
    let (_w, mut payer, _shop, key) = Box::pin(granted_then_lost()).await;
    Box::pin(payer.end_allowance(&key)).await.unwrap();
    let view = payer.allowance(&key).await.unwrap();
    assert_eq!(view.status, AllowanceStatus::Ended);
    assert_eq!(
        refusal_of(
            Box::pin(payer.confirm_allowance_executor(&view.hold, UserConfirmed::new())).await
        ),
        Refusal::AllowanceEnded
    );
}

#[tokio::test]
async fn test_allowance_regrant_never_granted_before_a_loss_is_not_automatic() {
    // Accepted before the loss but never granted here: it can't be granted now (Paykit spec
    // 8.3, Acceptance), so it pays nothing automatically and asks no confirmation.
    let (w, payer, shop, key) = Box::pin(super::accepted_not_granted(store())).await;
    let (_w, mut payer, _shop, key) = Box::pin(lost(w, payer, shop, key)).await;
    let view = payer.allowance(&key).await.unwrap();
    assert!(!view.confirmation_needed, "{view:?}");
    let set_up_before = Refusal::NotAutomatic(
        "the Allowance was set up before a provider loss and never granted here, so it can't be granted now: the user can end it and set up a new one".into(),
    );
    assert_eq!(
        refusal_of(Box::pin(payer.resume_allowance_executor(&key)).await),
        set_up_before
    );
    assert_eq!(
        refusal_of(
            Box::pin(payer.confirm_allowance_executor(&view.hold, UserConfirmed::new())).await
        ),
        set_up_before
    );
}
