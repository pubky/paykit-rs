//! A provider loss (Paykit spec 7.3, "After a provider loss"; Pubky Switch spec 12.6): the
//! signer declares alice's generation lost at a new service, the old one stops answering,
//! and her apps follow the loss there.

use std::sync::Arc;

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    Doubtful, LossBoundary, PayOptions, PayOutcome, Paykit, PaykitBackup, PaykitConfig,
    PaykitError, PaymentDoubt, Rail, RailDeclaration, Refusal, RequestRef, RequestStatus,
};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::{ScriptedSwitch, SWITCH_ORIGIN};
use pubky_switch_core::claims::ClaimStatus;
use pubky_switch_core::records::{Inbox, CLOCK_TOLERANCE_MS};

use crate::common::{draft, request_from, store, World, ALICE_ORIGIN};
use crate::restore::{is_refused, layer_of, nothing_restored, refused_restore, with_layer};

/// The new service's key secret.
const NEXT_SECRET: [u8; 32] = [0x4e; 32];

/// The key secret of the service after a second loss.
const LATER_SECRET: [u8; 32] = [0x4f; 32];

/// Alice's signer declares her current generation lost (Switch spec 12.6 step 1): a new
/// service recovering the account at the next generation, routed at [`SWITCH_ORIGIN`], the
/// next `identity.json` naming it with `recovery_from`, delivered there and published; then
/// the old service is unreachable.
fn declare_loss(w: &World) {
    declare_loss_of(w, ALICE_ORIGIN, NEXT_SECRET);
}

/// [`declare_loss`] of the service at `lost_origin`, the new one under `secret`.
fn declare_loss_of(w: &World, lost_origin: &str, secret: [u8; 32]) -> Arc<ScriptedSwitch> {
    let lost_generation = w
        .homeserver
        .identity_doc(&w.alice_id())
        .unwrap()
        .generation();
    let next = ScriptedSwitch::recovering(
        w.alice.identity().key(),
        secret,
        Arc::clone(&w.clock),
        lost_generation + 1,
        lost_generation,
    )
    .unwrap();
    let doc = w
        .alice
        .sign_next(|body| {
            body.generation = lost_generation + 1;
            body.inbox = Inbox::new(SWITCH_ORIGIN, next.service_key());
            body.recovery_from = Some(lost_generation);
        })
        .unwrap();
    next.deliver_identity(&doc).unwrap();
    w.router.remove(lost_origin);
    w.router.add_switch(SWITCH_ORIGIN, &next);
    next
}

/// `app` follows the loss: its own `identity.json` from the Homeserver, then reads at the new
/// service until its steps of the recovery have run, and its reconciliation ends. Returns the
/// end of the loss's residual window.
async fn follow_loss(app: &mut Paykit) -> u64 {
    app.app_mut().refresh_identity().await.unwrap();
    for _ in 0..4 {
        app.receive(None).await.unwrap();
    }
    let info = app.app().recovery_info().await.unwrap().unwrap();
    assert!(info.steps_done, "{info:?}");
    assert!(info.loss.uncertain_until_ms > info.loss.declared_at_ms);
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    info.loss.uncertain_until_ms
}

/// The doubt a payment was refused with, as [`Refusal::MayBePaid`] shows it.
fn doubt_of(paid: &paykit_sdk::Result<PayOutcome>) -> PaymentDoubt {
    match paid.as_ref().err().and_then(PaykitError::refusal) {
        Some(Refusal::MayBePaid {
            doubt: Some(doubt), ..
        }) => (**doubt).clone(),
        _ => panic!("not refused as may be paid: {paid:?}"),
    }
}

/// The provider loss `app` keeps (`p/pl`).
async fn kept_loss(app: &Paykit) -> LossBoundary {
    let kept: serde_json::Value =
        serde_json::from_slice(&app.app().protocol_get(b"pl").await.unwrap().unwrap()).unwrap();
    serde_json::from_value(kept["d"].clone()).unwrap()
}

/// A provider loss no app followed.
fn some_loss() -> LossBoundary {
    serde_json::from_value(serde_json::json!({
        "generation": 2,
        "declared_at_ms": 1,
        "uncertain_until_ms": 2,
    }))
    .unwrap()
}

/// The generation of the loss `doubt` names.
fn shown_generation(doubt: &PaymentDoubt) -> u64 {
    doubt.before_loss.unwrap().generation()
}

/// Every payment the method carried out, repeats included.
fn executions(w: &World) -> u64 {
    w.rail.all_executions().values().sum()
}

async fn status(app: &Paykit, request: &RequestRef) -> RequestStatus {
    app.request(request).await.unwrap().status
}

#[tokio::test]
async fn test_provider_loss_request_paid_at_the_old_service_needs_confirmation_before_paying_again()
{
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    // The phone pays the request at the old service, and its claim there is SETTLED. The
    // wallet reads nothing after it, and bob's shop receives nothing: no receipt comes.
    let paid = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let obligation = phone.payments().await.unwrap().remove(0).obligation;
    let at_old = phone
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(at_old.view.status, ClaimStatus::Settled);
    assert_eq!(executions(&w), 1);
    // The phone is gone with its state: nothing it held reaches the new service.
    drop(phone);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    // The new service holds no claim for the obligation: nothing settled it there.
    assert!(wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none_or(|read| read.view.status != ClaimStatus::Settled));
    // The request was payable before the declaration: paying it by hand needs the user's
    // confirmation that another app may already have paid it (Switch spec 12.6 step 5).
    let again = wallet.pay_request(&request, PayOptions::default()).await;
    let doubt = doubt_of(&again);
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    assert_eq!(doubt.obligation, obligation);
    assert_eq!(executions(&w), 1);
    // The user has seen it and pays anyway.
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 2);
}

#[tokio::test]
async fn test_provider_loss_own_attempt_before_the_loss_stays_in_doubt() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // This app attempts the request at the old service, and the method reports it failed:
    // the claim there is FREE again, its attempt `failed`.
    w.rail.next(Behaviour::Fail);
    let failed = wallet.pay_request(&request, PayOptions::default()).await;
    assert!(!matches!(failed, Ok(PayOutcome::Paid { .. })), "{failed:?}");
    assert_eq!(executions(&w), 0);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    // Another own app may have paid it at the old service meanwhile: this app's own attempt
    // there settles nothing about that.
    let again = wallet.pay_request(&request, PayOptions::default()).await;
    assert!(doubt_of(&again).before_loss.is_some());
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_automatic_payment_of_an_obligation_from_before_is_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let dispatched = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let held = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // A dispatch before the loss whose answer is lost: the method paid it, and the attempt
    // stays open for a resume to repeat.
    w.rail.set_idempotent(true);
    w.rail.next(Behaviour::LoseAnswer);
    let lost = wallet
        .pay_request(&dispatched, PayOptions::default())
        .await
        .unwrap();
    assert!(!matches!(lost, PayOutcome::Paid { .. }), "{lost:?}");
    assert_eq!(executions(&w), 1);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    // A resume never repeats the dispatch made before the loss.
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        !resumed
            .iter()
            .any(|outcome| matches!(outcome, PayOutcome::Paid { .. })),
        "{resumed:?}"
    );
    assert_eq!(executions(&w), 1);
    // An automatic payment of a request from before the loss is refused, a confirmation of
    // its doubt passing nothing.
    let refused = wallet
        .pay_request_automatically(&held, PayOptions::default())
        .await;
    let doubt = doubt_of(&refused);
    assert!(doubt.before_loss.is_some(), "{doubt:?}");
    let refused = wallet
        .pay_request_automatically(
            &held,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await;
    assert!(doubt_of(&refused).before_loss.is_some());
    assert_eq!(executions(&w), 1);
}

/// Alice's wallet restored from `backup` on a store of its own, the device the backup came
/// from gone.
async fn restored(w: &World, backup: &PaykitBackup) -> Paykit {
    Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap(),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn test_provider_loss_doubt_survives_a_restore_from_a_backup_after_the_loss() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    app.receive(None).await.unwrap();
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    // The restore holds the request until the user has seen the payee may have cancelled
    // it; the loss's doubt comes after that, carried by the backup.
    let options = PayOptions::default().with_possibly_cancelled_confirmed(UserConfirmed::new());
    let again = app.pay_request(&request, options).await;
    assert!(doubt_of(&again).before_loss.is_some(), "{again:?}");
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_restore_from_a_backup_before_the_loss_learns_it_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut laptop = w.wallet("laptop").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    laptop.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    // The laptop follows the loss: the request's record reaches the new service.
    follow_loss(&mut laptop).await;
    // Restored from a backup taken before the loss, the app learns of it when it follows it.
    let mut app = restored(&w, &backup).await;
    assert!(app.app().protocol_get(b"pl").await.unwrap().is_none());
    follow_loss(&mut app).await;
    let kept: serde_json::Value =
        serde_json::from_slice(&app.app().protocol_get(b"pl").await.unwrap().unwrap()).unwrap();
    let loss = app.app().recovery_info().await.unwrap().unwrap().loss;
    assert_eq!(kept["d"]["declared_at_ms"], loss.declared_at_ms);
    assert_eq!(kept["d"]["uncertain_until_ms"], loss.uncertain_until_ms);
    app.receive(None).await.unwrap();
    let options = PayOptions::default().with_possibly_cancelled_confirmed(UserConfirmed::new());
    let again = app.pay_request(&request, options).await;
    assert!(doubt_of(&again).before_loss.is_some(), "{again:?}");
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_doubt_outlasts_the_residual_window() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let before = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    let uncertain_until_ms = follow_loss(&mut wallet).await;
    // Hours past the residual window, the request still payable: the window decides which
    // obligations are uncertain, not how long the doubt lasts.
    let now = w.clock.now_ms();
    w.clock
        .advance_ms(uncertain_until_ms.saturating_sub(now) + 6 * 3_600_000);
    let after = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let again = wallet.pay_request(&before, PayOptions::default()).await;
    assert!(doubt_of(&again).before_loss.is_some(), "{again:?}");
    assert_eq!(executions(&w), 0);
    // A request created after the declaration plus the clock tolerance isn't uncertain.
    assert_eq!(status(&wallet, &after).await, RequestStatus::Open);
    let paid = wallet
        .pay_request(&after, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_confirm_not_paid_before_loss_clears_that_obligation_only() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let first_doubt = doubt_of(&wallet.pay_request(&first, PayOptions::default()).await);
    let second_doubt = doubt_of(&wallet.pay_request(&second, PayOptions::default()).await);
    // A payment the user confirms over the doubt that fails states nothing: the second
    // request stays in doubt.
    w.rail.next(Behaviour::Fail);
    let failed = wallet
        .pay_request(
            &second,
            PayOptions::default().with_may_be_paid_confirmed(second_doubt),
        )
        .await;
    assert!(!matches!(failed, Ok(PayOutcome::Paid { .. })), "{failed:?}");
    assert!(
        doubt_of(&wallet.pay_request(&second, PayOptions::default()).await)
            .before_loss
            .is_some()
    );
    // The user states the first wasn't paid before the loss: it is paid as any other.
    wallet
        .confirm_not_paid_before_loss(
            &first_doubt.obligation,
            &first_doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert_eq!(status(&wallet, &first).await, RequestStatus::Open);
    assert_eq!(status(&wallet, &second).await, RequestStatus::MayBePaid);
    let paid = wallet
        .pay_request(&first, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert!(
        doubt_of(&wallet.pay_request(&second, PayOptions::default()).await)
            .before_loss
            .is_some()
    );
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_settled_claim_at_the_new_service_ends_the_doubt() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut laptop = w.wallet("laptop").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    laptop.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    follow_loss(&mut laptop).await;
    assert_eq!(status(&laptop, &request).await, RequestStatus::MayBePaid);
    // The wallet pays it over the doubt: its claim at the new service is SETTLED.
    let doubt = doubt_of(&wallet.pay_request(&request, PayOptions::default()).await);
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    // The laptop's doubt ends with it: the request reads paid, never "may be paid".
    laptop.receive(None).await.unwrap();
    assert_eq!(status(&laptop, &request).await, RequestStatus::Paid);
    let again = laptop.pay_request(&request, PayOptions::default()).await;
    assert!(
        matches!(
            again.as_ref().err().and_then(PaykitError::refusal),
            Some(Refusal::AlreadyPaid)
        ),
        "{again:?}"
    );
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_request_created_inside_the_residual_window_is_paid_only_by_hand() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // An earlier request of bob's, which the loss's steps carry to the new service.
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    let uncertain_until_ms = follow_loss(&mut wallet).await;
    let loss = wallet.app().recovery_info().await.unwrap().unwrap().loss;
    // Bob's request is signed after the declaration plus the clock tolerance, but before the
    // end of the residual window plus the tolerance, and reaches the wallet only after the
    // window: the old service could have taken another own app's attempt on it.
    let created = uncertain_until_ms + CLOCK_TOLERANCE_MS - 1_000;
    assert!(created >= loss.declared_at_ms + CLOCK_TOLERANCE_MS);
    w.clock.advance_ms(created.saturating_sub(w.clock.now_ms()));
    let request = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    // Never paid automatically, a confirmation of its doubt passing nothing.
    let refused = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await;
    let doubt = doubt_of(&refused);
    assert!(doubt.before_loss.is_some(), "{doubt:?}");
    let refused = wallet
        .pay_request_automatically(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await;
    assert!(doubt_of(&refused).before_loss.is_some());
    // By hand, it is met by the prompt that it may already be paid, and goes only on the
    // user's confirmation.
    let doubt = doubt_of(&wallet.pay_request(&request, PayOptions::default()).await);
    assert!(doubt.before_loss.is_some(), "{doubt:?}");
    assert_eq!(executions(&w), 0);
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_resume_never_repeats_a_dispatch_from_before_the_loss() {
    let w = World::new();
    w.rail.set_declaration(
        RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS).with_idempotent(true),
    );
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // Dispatched at the old service, its answer never coming: a resume on this idempotent
    // method without a history would repeat it.
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    let obligation = wallet.payments().await.unwrap().remove(0).obligation;
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    // The new service's claim shows this app's attempt from before the loss open: the one
    // a resume would repeat.
    let read = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    let attempt = read.view.attempt(&attempt_id.to_bytes()).unwrap();
    assert_eq!(attempt.owner, wallet.app_id());
    assert_eq!(attempt.outcome, pubky_switch_core::claims::Outcome::Open);
    // The user states it wasn't paid before the loss: the obligation's doubt ends, and
    // still the dispatch this app made before the loss isn't repeated.
    wallet
        .confirm_not_paid_before_loss(&obligation, &kept_loss(&wallet).await, UserConfirmed::new())
        .await
        .unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        resumed.iter().any(|outcome| matches!(
            outcome,
            PayOutcome::Uncertain { attempt_id: id, .. } if *id == attempt_id
        )),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_provider_loss_newer_loss_puts_a_stated_obligation_in_doubt_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let doubt = doubt_of(&wallet.pay_request(&request, PayOptions::default()).await);
    wallet
        .confirm_not_paid_before_loss(
            &doubt.obligation,
            &doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert_eq!(status(&wallet, &request).await, RequestStatus::Open);
    // The new service is lost too: what the user stated was about the first loss.
    w.clock.advance_ms(60_000);
    declare_loss_of(&w, SWITCH_ORIGIN, LATER_SECRET);
    follow_loss(&mut wallet).await;
    let loss = wallet.app().recovery_info().await.unwrap().unwrap().loss;
    let kept: serde_json::Value =
        serde_json::from_slice(&wallet.app().protocol_get(b"pl").await.unwrap().unwrap()).unwrap();
    assert_eq!(kept["d"]["generation"], loss.generation);
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    let again = wallet.pay_request(&request, PayOptions::default()).await;
    assert!(doubt_of(&again).before_loss.is_some(), "{again:?}");
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_confirm_not_paid_before_loss_refused_without_a_loss_kept() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // No loss followed: nothing to state, and nothing is written.
    let refused = wallet
        .confirm_not_paid_before_loss(&[7; 32], &some_loss(), UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    assert!(wallet.app().protocol_scan(b"pn/").await.unwrap().is_empty());
    assert_eq!(status(&wallet, &request).await, RequestStatus::Open);
}

#[tokio::test]
async fn test_provider_loss_restore_refuses_a_loss_above_the_clients_generation() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    let id = wallet.app_id();
    drop(wallet);
    let backup: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let followed = backup["meta"]["generation"].as_u64().unwrap();
    // The kept loss as no run writes it: recovered at a generation after the client's.
    let mut raised = false;
    let state = layer_of(&backup)
        .into_iter()
        .map(|(key, mut value)| {
            if key == b"pl" {
                assert_eq!(value["d"]["generation"], followed);
                value["d"]["generation"] = serde_json::json!(followed + 1);
                raised = true;
            }
            (key, value)
        })
        .collect();
    assert!(raised);
    let backup = with_layer(&backup, state);
    let at = store();
    let refused = refused_restore(&w, backup, Arc::clone(&at)).await;
    assert!(is_refused(&refused), "{refused:?}");
    nothing_restored(&w, at, id).await;
}

/// The obligations a reconcile of `app` lists as in doubt after a provider loss.
async fn loss_doubts(app: &mut Paykit) -> Vec<Doubtful> {
    let report = app.reconcile().await.unwrap();
    report
        .doubtful
        .into_iter()
        .filter(|doubtful| doubtful.before_loss.is_some())
        .collect()
}

/// A wallet holding two of bob's requests, received before alice's service is lost and
/// followed there. Returns the world, the wallet and the requests.
async fn followed_with_requests() -> (World, Paykit, [RequestRef; 2]) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    (w, wallet, [first, second])
}

#[tokio::test]
async fn test_provider_loss_reconcile_lists_each_obligation_from_before_the_loss() {
    // Neither request was attempted at the new service: the reconcile lists both as in
    // doubt after the loss, with their payee and request and no attempt, the obligation the
    // refusal of a payment by hand names.
    let (w, mut wallet, requests) = followed_with_requests().await;
    let listed = loss_doubts(&mut wallet).await;
    assert_eq!(listed.len(), 2, "{listed:?}");
    for request in &requests {
        let doubtful = listed
            .iter()
            .find(|d| d.request_hash == Some(request.request_hash))
            .unwrap();
        assert_eq!(doubtful.payee, Some(w.bob_id()));
        assert!(doubtful.attempts.is_empty(), "{doubtful:?}");
        let refused = wallet.pay_request(request, PayOptions::default()).await;
        assert_eq!(doubt_of(&refused).obligation, doubtful.obligation);
    }
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_reconcile_lists_no_obligation_the_user_stated_not_paid() {
    let (_w, mut wallet, [first, second]) = followed_with_requests().await;
    let doubt = doubt_of(&wallet.pay_request(&first, PayOptions::default()).await);
    wallet
        .confirm_not_paid_before_loss(
            &doubt.obligation,
            &doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    let listed = loss_doubts(&mut wallet).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].request_hash, Some(second.request_hash));
}

#[tokio::test]
async fn test_provider_loss_reconcile_lists_no_obligation_settled_at_the_new_service() {
    let (w, mut wallet, [first, second]) = followed_with_requests().await;
    let doubt = doubt_of(&wallet.pay_request(&first, PayOptions::default()).await);
    let paid = wallet
        .pay_request(
            &first,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let listed = loss_doubts(&mut wallet).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].request_hash, Some(second.request_hash));
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_reconcile_lists_a_stated_obligation_again_after_a_newer_loss() {
    let (w, mut wallet, [first, _]) = followed_with_requests().await;
    let doubt = doubt_of(&wallet.pay_request(&first, PayOptions::default()).await);
    wallet
        .confirm_not_paid_before_loss(
            &doubt.obligation,
            &doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert_eq!(loss_doubts(&mut wallet).await.len(), 1);
    w.clock.advance_ms(60_000);
    declare_loss_of(&w, SWITCH_ORIGIN, LATER_SECRET);
    follow_loss(&mut wallet).await;
    let listed = loss_doubts(&mut wallet).await;
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(listed
        .iter()
        .any(|d| d.request_hash == Some(first.request_hash) && d.obligation == doubt.obligation));
}

/// A request from before a loss, its doubt shown to the user under that loss, then a newer
/// loss followed: returns the doubt shown.
async fn shown_then_newer_loss(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
    step_run: bool,
) -> (RequestRef, PaymentDoubt) {
    w.clock.advance_ms(60_000);
    declare_loss(w);
    follow_loss(wallet).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    // Still inside the first loss's residual window: the request is doubtful, and its
    // record is written at this service before the next loss.
    let request = request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let shown = doubt_of(&wallet.pay_request(&request, PayOptions::default()).await);
    w.clock.advance_ms(60_000);
    let next = declare_loss_of(w, SWITCH_ORIGIN, LATER_SECRET);
    if step_run {
        follow_loss(wallet).await;
    } else {
        next.fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
        wallet.app_mut().refresh_identity().await.unwrap();
        let mut following = None;
        for _ in 0..4 {
            let info = wallet.app().recovery_info().await.unwrap().unwrap();
            if info.loss.generation > shown_generation(&shown) && !info.steps_done {
                following = Some(info);
                break;
            }
            let _ = wallet.receive(None).await;
        }
        assert!(
            following.is_some(),
            "the newer loss isn't followed before its step"
        );
    }
    (request, shown)
}

#[tokio::test]
async fn test_provider_loss_confirmed_doubt_of_an_older_loss_pays_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, shown) = shown_then_newer_loss(&w, &mut wallet, &mut shop, true).await;
    // The user confirms the doubt shown under the first loss: the newer loss decides now,
    // so the payment is refused with its doubt, and nothing is paid.
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(shown.clone()),
        )
        .await;
    let now = doubt_of(&paid);
    assert!(shown_generation(&now) > shown_generation(&shown), "{now:?}");
    assert_eq!(executions(&w), 0);
    // The doubt shown under the newer loss, confirmed, pays.
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(now),
        )
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_statement_about_an_older_loss_refused_after_a_newer_one() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, shown) = shown_then_newer_loss(&w, &mut wallet, &mut shop, true).await;
    // The user's answer to the prompt shown under the first loss comes late: it is refused,
    // naming the newer loss, and the obligation stays in doubt after it.
    let decisive = kept_loss(&wallet).await;
    let refused = wallet
        .confirm_not_paid_before_loss(
            &shown.obligation,
            &shown.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        refused.refusal(),
        Some(&Refusal::LossChanged {
            decisive: Some(decisive)
        })
    );
    assert!(wallet.app().protocol_scan(b"pn/").await.unwrap().is_empty());
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    assert_eq!(executions(&w), 0);
    // The answer to the doubt shown under the newer loss ends it.
    let again = doubt_of(&wallet.pay_request(&request, PayOptions::default()).await);
    assert_eq!(again.before_loss, Some(decisive));
    wallet
        .confirm_not_paid_before_loss(&again.obligation, &decisive, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(status(&wallet, &request).await, RequestStatus::Open);
}

#[tokio::test]
async fn test_provider_loss_statement_about_an_older_loss_refused_before_the_newer_ones_step() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, shown) = shown_then_newer_loss(&w, &mut wallet, &mut shop, false).await;
    // The newer loss is followed, its step not run yet: an answer about the first loss is
    // refused, not taken for a statement the newer loss's step drops.
    let refused = wallet
        .confirm_not_paid_before_loss(
            &shown.obligation,
            &shown.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(Refusal::LossChanged { decisive: Some(loss) })
                if loss.generation() > shown_generation(&shown)
        ),
        "{refused:?}"
    );
    assert!(wallet.app().protocol_scan(b"pn/").await.unwrap().is_empty());
    follow_loss(&mut wallet).await;
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    assert_eq!(executions(&w), 0);
}
