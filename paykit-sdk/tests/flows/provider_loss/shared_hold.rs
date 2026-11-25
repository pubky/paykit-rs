//! The shared post-loss hold (Paykit spec 8.1, Blocked counterparties; Paykit wire 5.6): once
//! a provider loss is declared, no own app pays a one-time request automatically, nor repeats
//! a one-time payment on a resume, for good. A payment the user starts by hand still goes.

use std::sync::Arc;

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{NoticeKind, PayOptions, PayOutcome, Paykit, PaykitConfig, Rail, RailDeclaration};
use pubky_dispatch_client::app::UserConfirmed;
use pubky_dispatch_client::clock::Clock;
use pubky_dispatch_core::records::Inbox;

use super::{declare_loss, declare_second_loss, doubt_of, executions, follow_loss};
use crate::common::{draft, request_from, store, World};

/// Past the loss's residual window, so that only the shared hold, not the doubt of an
/// obligation from before the loss, can hold a new request.
fn past_the_window(w: &World, uncertain_until_ms: u64) {
    let now = w.clock.now_ms();
    w.clock
        .advance_ms(uncertain_until_ms.saturating_sub(now) + 3_600_000);
}

use super::held_after_loss as held;

/// Bob's request before the loss, which makes him a counterparty the loss's steps carry to the
/// new service, and the wallet's read of it.
async fn earlier_request(w: &World, wallet: &mut Paykit, shop: &mut Paykit) {
    request_from(shop, w.alice_id(), draft("0", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
}

/// The recorded loss `app` keeps (`rl`), as stored.
async fn recorded(app: &Paykit) -> Option<Vec<u8>> {
    app.app().protocol_get("paykit", b"rl").await.unwrap()
}

/// The shared hold notices `app` shows.
async fn hold_notices(app: &Paykit) -> Vec<paykit_sdk::Notice> {
    app.notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|notice| notice.kind == NoticeKind::AutomaticPaymentsHeld)
        .collect()
}

#[tokio::test]
async fn test_shared_hold_request_after_the_loss_paid_only_by_hand() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    earlier_request(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    let until = follow_loss(&mut wallet).await;
    past_the_window(&w, until);
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // Created long after the loss, outside its doubt: still never paid automatically.
    for request in [&first, &second] {
        let outcome = wallet
            .pay_request_automatically(request, PayOptions::default())
            .await
            .outcome;
        assert!(held(&outcome), "{outcome:?}");
    }
    assert_eq!(executions(&w), 0);
    // By hand it goes, once, and the hold stays for the other.
    let paid = wallet
        .pay_request(&first, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
    let outcome = wallet
        .pay_request_automatically(&second, PayOptions::default())
        .await
        .outcome;
    assert!(held(&outcome), "{outcome:?}");
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_shared_hold_statement_lifts_no_automatic_payment() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let doubt = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    wallet
        .confirm_not_paid_before_loss(
            &doubt.obligation,
            &doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    // The statement ends the doubt for a payment by hand, never the automatic hold.
    let outcome = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome;
    assert!(held(&outcome), "{outcome:?}");
    assert_eq!(executions(&w), 0);
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_shared_hold_no_repeat_of_an_attempt_the_user_started() {
    let w = World::new();
    w.rail.set_declaration(
        RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true),
    );
    let mut wallet = w
        .wallet_with("wallet", store(), crate::common::day_window())
        .await;
    let mut shop = w.shop().await;
    earlier_request(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    let until = follow_loss(&mut wallet).await;
    past_the_window(&w, until);
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("expected a pending payment, got {outcome:?}");
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    // A resume's repeat is an automatic payment, the user's own attempt included.
    w.clock.advance_ms(60_000);
    wallet.resume_payments().await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_shared_hold_second_and_restored_apps_hold() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut laptop = w.wallet("laptop").await;
    let mut shop = w.shop().await;
    earlier_request(&w, &mut wallet, &mut shop).await;
    laptop.receive(None).await.unwrap();
    let before = wallet.backup().await.unwrap();
    drop(wallet);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    // The laptop follows the loss; the wallet is restored on a fresh device from its backup
    // taken before it, and follows it there.
    let until = follow_loss(&mut laptop).await;
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &before,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert!(restored.app().own_identity().body().recovery_from.is_none());
    follow_loss(&mut restored).await;
    assert!(restored.app().own_identity().body().recovery_from.is_some());
    past_the_window(&w, until);
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    for app in [&mut laptop, &mut restored] {
        app.receive(None).await.unwrap();
        let outcome = app
            .pay_request_automatically(&request, PayOptions::default())
            .await
            .outcome;
        assert!(held(&outcome), "{outcome:?}");
    }
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_shared_hold_key_change_without_loss_holds_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let changed = w.alice.dispatch().change_key_into([74; 32]);
    w.router.add_dispatch(crate::common::ALICE_ORIGIN, &changed);
    let current = w
        .alice
        .sign_next(|body| {
            body.generation += 1;
            body.inbox = Inbox::new(crate::common::ALICE_ORIGIN, changed.service_key());
        })
        .unwrap();
    assert_eq!(current.body().recovery_from, None);
    changed.deliver_identity(&current).unwrap();
    wallet.app_mut().refresh_identity().await.unwrap();
    wallet.app_mut().follow_provider().await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.app().own_identity().generation(),
        current.generation()
    );
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
    assert!(hold_notices(&wallet).await.is_empty());
}

#[tokio::test]
async fn test_shared_hold_kept_through_a_key_change_and_a_second_loss() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    earlier_request(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(60_000);
    let next = declare_loss(&w);
    let until = follow_loss(&mut wallet).await;
    past_the_window(&w, until);
    // A key change in place after the loss keeps `recovery_from` below `generation - 1`.
    let changed = next.change_key_into([75; 32]);
    w.router
        .add_dispatch(pubky_dispatch_client::testing::SWITCH_ORIGIN, &changed);
    let current = w
        .alice
        .sign_next(|body| {
            body.generation += 1;
            body.inbox = Inbox::new(
                pubky_dispatch_client::testing::SWITCH_ORIGIN,
                changed.service_key(),
            );
        })
        .unwrap();
    let lost = current.body().recovery_from.unwrap();
    assert!(lost < current.generation() - 1);
    changed.deliver_identity(&current).unwrap();
    wallet.app_mut().refresh_identity().await.unwrap();
    wallet.app_mut().follow_provider().await.unwrap();
    wallet.receive(None).await.unwrap();
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome;
    assert!(held(&outcome), "{outcome:?}");
    // And a second loss.
    w.clock.advance_ms(60_000);
    declare_second_loss(&w);
    let until = follow_loss(&mut wallet).await;
    past_the_window(&w, until);
    let later = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request_automatically(&later, PayOptions::default())
        .await
        .outcome;
    assert!(held(&outcome), "{outcome:?}");
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_shared_hold_host_told_once_and_after_each_reopen() {
    let w = World::new();
    let storage = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&storage)).await;
    assert!(hold_notices(&wallet).await.is_empty());
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let shown = hold_notices(&wallet).await;
    assert_eq!(shown.len(), 1);
    assert!(shown[0].counterparty.is_none() && shown[0].attempt_id.is_none());
    wallet.acknowledge_notice(&shown[0].id).await.unwrap();
    // Once per load: later reads tell nothing more.
    wallet.receive(None).await.unwrap();
    assert!(hold_notices(&wallet).await.is_empty());
    // Reopened, it is told again.
    let app_id = wallet.app_id();
    drop(wallet);
    let wallet = w
        .load_on_clock(&w.alice, Arc::clone(&storage), app_id, Arc::clone(&w.clock))
        .await;
    assert_eq!(hold_notices(&wallet).await.len(), 1);
}

#[tokio::test]
async fn test_shared_hold_in_place_restore_keeps_the_recorded_loss() {
    let w = World::new();
    let storage = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&storage)).await;
    let before = wallet.backup().await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let kept = recorded(&wallet).await;
    assert!(kept.is_some());
    drop(wallet);
    // The backup from before the loss holds no recorded loss; the installation's is kept.
    let restored = Box::pin(Paykit::restore(
        w.env(&w.alice, Arc::clone(&storage)),
        PaykitConfig::default(),
        &before,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert_eq!(recorded(&restored).await, kept);
    // Before it reads anything, its `identity.json` is the backup's, from before the loss:
    // the recorded loss alone holds it, and the host is told.
    assert!(restored.app().own_identity().body().recovery_from.is_none());
    assert_eq!(hold_notices(&restored).await.len(), 1);
    drop(restored);
    // A fresh device has no installation: it learns the loss from the identity.
    let fresh = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &before,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert_eq!(recorded(&fresh).await, None);
    assert!(hold_notices(&fresh).await.is_empty());
}

#[tokio::test]
async fn test_shared_hold_with_a_divergent_floor_unblocks_without_a_raise() {
    // Paykit wire 5.6, Unblocking under the divergence hold: once the floor is held divergent
    // after a loss, the shared post-loss hold stands too, so the unblock goes with no floor
    // write. Without the shared hold, the divergent floor holds it
    // (`blocked_return::test_blocked_return_divergent_floor_held_for_good`).
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
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let mut phone = w
        .join_with(
            &w.alice,
            "phone",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    w.clock.advance_ms(60_000);
    let next = declare_loss(&w);
    follow_loss(&mut wallet).await;
    follow_loss(&mut phone).await;
    let target = pubky_dispatch_client::app::RecordTarget::new(
        pubky_dispatch_client::app::Collection::new(
            "block_floor",
            paykit_lib::collections::Collection::BlockFloor.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::block_floor(&w.alice_id()),
    );
    phone
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    next.make_divergent([0x5a; 16], key, [1; 32]).unwrap();
    wallet.receive(None).await.unwrap();
    let floor_before = next.record(&key).map(|held| held.revision);
    wallet.unblock(&w.carol_id()).await.unwrap();
    assert!(!wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.carol_id()));
    // No floor write by the wallet.
    assert_eq!(next.record(&key).map(|held| held.revision), floor_before);
}

#[tokio::test]
async fn test_shared_hold_lets_an_owed_unblock_go_once_it_stands() {
    // Paykit wire 5.6, Unblocking under the divergence hold: the floor is held divergent and
    // another app unblocks carol, so the wallet owes a raise it can't write. Once a declared
    // loss adds the shared post-loss hold, the owed unblock goes without the raise at the next
    // receive, and nothing waits for it any more.
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
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let target = pubky_dispatch_client::app::RecordTarget::new(
        pubky_dispatch_client::app::Collection::new(
            "block_floor",
            paykit_lib::collections::Collection::BlockFloor.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::block_floor(&w.alice_id()),
    );
    wallet
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    w.alice
        .dispatch()
        .make_divergent([0x5b; 16], key, [2; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            paykit_sdk::AppScopes::new()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.unblock(&w.carol_id()).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(11 * 60_000);
        wallet.receive(None).await.unwrap();
    }
    assert!(unblock_owed(&wallet).await);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    w.clock.advance_ms(5_000);
    wallet.receive(None).await.unwrap();
    assert!(!unblock_owed(&wallet).await);
}

/// Whether `app` owes the raise of an unblock (`bf`).
async fn unblock_owed(app: &Paykit) -> bool {
    app.app()
        .protocol_get("paykit", b"bf")
        .await
        .unwrap()
        .is_some()
}

#[tokio::test]
async fn test_shared_hold_unblocks_under_both_holds_while_the_floor_cant_be_read() {
    // Paykit spec 8.1; Paykit wire 5.6, Unblocking under the divergence hold: under both
    // holds the user's unblock goes without the raise even while the floor can't be read,
    // here while a wider certificate's scope keys are awaited.
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
    let me = wallet.app_id();
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let target = pubky_dispatch_client::app::RecordTarget::new(
        pubky_dispatch_client::app::Collection::new(
            "block_floor",
            paykit_lib::collections::Collection::BlockFloor.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::block_floor(&w.alice_id()),
    );
    wallet
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    w.alice
        .dispatch()
        .make_divergent([0x5c; 16], key, [3; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    let next = declare_loss(&w);
    follow_loss(&mut wallet).await;
    // Renewed at the new service to read more, the new scope's keys not given: the floor
    // can't be read now.
    let first = wallet.app().cert().clone();
    let scopes = paykit_sdk::AppScopes::new()
        .pay()
        .manage_contacts()
        .request()
        .build()
        .unwrap();
    let names: Vec<&str> = scopes.scopes.iter().map(String::as_str).collect();
    let wider = pubky_dispatch_client::testing::issue_cert(
        w.alice.identity().key(),
        &w.alice.identity().app_keys("wallet"),
        &pubky_dispatch_client::testing::AppSpec::new(&names).with_permissions(&scopes.permissions),
        w.clock.now_ms() + 1_000,
    )
    .unwrap();
    next.signer_op(pubky_dispatch_core::ops::OpKind::CertPut {
        target_app_id: me,
        cert: wider.signed().clone(),
    })
    .unwrap();
    let _ = wallet.receive(None).await;
    let doc = w
        .alice
        .sign_next(|body| {
            body.apps.retain(|listed| listed.app_id != me);
            body.apps.push(pubky_dispatch_core::records::ListedApp {
                app_id: me,
                cert: wider.hash(),
            });
            body.renewed
                .push(pubky_dispatch_core::records::RenewedCert {
                    app_id: me,
                    cert: first.hash(),
                    until: None,
                });
        })
        .unwrap();
    next.deliver_identity(&doc).unwrap();
    for _ in 0..2 {
        let _ = wallet.receive(None).await;
    }
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert!(wallet.app().own_identity().body().recovery_from.is_some());
    wallet.unblock(&w.carol_id()).await.unwrap();
    assert!(!wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.carol_id()));
}

#[tokio::test]
async fn test_shared_hold_applies_a_restore_widening_without_the_raise() {
    // Paykit wire 5.6, Unblocking under the divergence hold: under both holds, the widening of
    // an in-place restore that isn't clean applies at once with no `block_floor` write, so it
    // owes nothing and holds no ask.
    let w = World::new();
    let storage = store();
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
            Arc::clone(&storage),
        )
        .await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let target = pubky_dispatch_client::app::RecordTarget::new(
        pubky_dispatch_client::app::Collection::new(
            "block_floor",
            paykit_lib::collections::Collection::BlockFloor.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::block_floor(&w.alice_id()),
    );
    wallet
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    w.alice
        .dispatch()
        .make_divergent([0x5d; 16], key, [4; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    let next = declare_loss(&w);
    follow_loss(&mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    crate::common::restore_with_unknown_cursor(&mut restored).await;
    let floor_before = next.record(&key).map(|held| held.revision);
    for _ in 0..2 {
        w.clock.advance_ms(11 * 60_000);
        restored.receive(None).await.unwrap();
    }
    // The widening applied: carol, whom the deny list holds, is marked as blocked.
    assert!(!restored
        .app()
        .protocol_scan("paykit", b"bd/")
        .await
        .unwrap()
        .is_empty());
    let widening_owed = restored
        .app()
        .protocol_get("paykit", b"rh")
        .await
        .unwrap()
        .is_some_and(|bytes| {
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            !value["d"]["widening_read"].is_null() || !value["d"]["earlier_widening"].is_null()
        });
    assert!(!widening_owed);
    assert_eq!(next.record(&key).map(|held| held.revision), floor_before);
}

#[tokio::test]
async fn test_shared_hold_recorded_loss_kept_across_a_scope_change() {
    // Paykit wire 5.6, Shared post-loss hold: the recorded loss stays through a renewal to
    // other scopes, and the hold with it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    earlier_request(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(60_000);
    let next = declare_loss(&w);
    let until = follow_loss(&mut wallet).await;
    let before = recorded(&wallet).await;
    assert!(before.is_some());
    let first = wallet.app().cert().clone();
    let scopes = paykit_sdk::AppScopes::new()
        .pay()
        .request()
        .build()
        .unwrap();
    let names: Vec<&str> = scopes.scopes.iter().map(String::as_str).collect();
    let wider = pubky_dispatch_client::testing::issue_cert(
        w.alice.identity().key(),
        &w.alice.identity().app_keys("wallet"),
        &pubky_dispatch_client::testing::AppSpec::new(&names).with_permissions(&scopes.permissions),
        w.clock.now_ms() + 1_000,
    )
    .unwrap();
    next.signer_op(pubky_dispatch_core::ops::OpKind::CertPut {
        target_app_id: me,
        cert: wider.signed().clone(),
    })
    .unwrap();
    // The signer stores a bundle with the scope keys the renewed certificate needs.
    let epoch = next.identity_doc().unwrap().data_epoch();
    for part in pubky_dispatch_core::records::seal_bundle_parts(
        w.alice.identity().key(),
        &wider,
        1,
        epoch,
        &|scope| crate::common::spec().has_keys(scope),
    )
    .unwrap()
    {
        next.signer_op(pubky_dispatch_core::ops::OpKind::KeyBundlePut(
            pubky_dispatch_core::ops::KeyBundlePut {
                target_app_id: me,
                bundle: part.bundle,
                first_epoch: part.first_epoch,
                last_epoch: part.last_epoch,
            },
        ))
        .unwrap();
    }
    let _ = wallet.receive(None).await;
    let doc = w
        .alice
        .sign_next(|body| {
            body.apps.retain(|listed| listed.app_id != me);
            body.apps.push(pubky_dispatch_core::records::ListedApp {
                app_id: me,
                cert: wider.hash(),
            });
            body.renewed
                .push(pubky_dispatch_core::records::RenewedCert {
                    app_id: me,
                    cert: first.hash(),
                    until: None,
                });
        })
        .unwrap();
    next.deliver_identity(&doc).unwrap();
    for _ in 0..3 {
        let _ = wallet.receive(None).await;
    }
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert_eq!(recorded(&wallet).await, before);
    past_the_window(&w, until);
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    crate::common::finish_listing(&mut wallet).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome;
    assert!(held(&outcome), "{outcome:?}");
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_shared_hold_lets_payments_by_hand_go_on() {
    // Paykit wire 5.6, Shared post-loss hold: past the loss's window, a one-time request
    // created after the loss is paid by hand, and a payment without a request goes, each
    // under its own rules. (A Billing Period from before the loss meets the loss's own doubt,
    // Paykit spec 7.3, not this hold.)
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    earlier_request(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    let until = follow_loss(&mut wallet).await;
    past_the_window(&w, until);
    let request = request_from(&mut shop, w.alice_id(), draft("9", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    crate::public::publish(
        &w,
        serde_json::json!({
            "capabilities": { "endpoint_identifiers": ["btc-lightning-bolt12"] },
            "public_endpoints": { "btc-lightning-bolt12": "lno1tips" }
        }),
        Some(w.clock.now_ms() + 30 * 24 * 3_600_000),
    );
    let payment = paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("0.001"))
        .with_options(
            PayOptions::default().with_mode(paykit_sdk::PaymentMode::Public(UserConfirmed::new())),
        );
    let unrequested = wallet.pay_without_request(payment).await.outcome.unwrap();
    assert!(
        matches!(unrequested, PayOutcome::Paid { .. }),
        "{unrequested:?}"
    );
    assert_eq!(executions(&w), 2);
}
