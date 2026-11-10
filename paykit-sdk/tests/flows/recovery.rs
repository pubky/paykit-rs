//! Recovering a payment a crash or a lost answer left open (Paykit spec 7.2, "What a payment
//! method declares", "No second execution"): what the payment method declares decides, and
//! a device clock that went back makes no finite retention look current.

use std::sync::Arc;

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, RailDeclaration, Stage};
use pubky_switch_client::clock::{Clock, TestClock};
use pubky_switch_core::claims::{ClaimStatus, Outcome};

use crate::common::{draft, request_from, store, World, HOUR_MS};

const DAY_MS: u64 = 24 * HOUR_MS;

/// A payment left in flight on a wallet on `base`, then the wallet dropped. Returns the
/// attempt and the wallet's App ID.
async fn in_flight(
    w: &World,
    base: &Arc<dyn pubky_switch_client::store::StateStore>,
) -> (paykit_lib::AttemptId, [u8; 16]) {
    let mut wallet = w.wallet_on("wallet", Arc::clone(base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    (attempt_id, wallet.app_id())
}

/// The device clock, an hour behind the time the payment was journaled.
fn clock_gone_back(w: &World) -> Arc<TestClock> {
    Arc::new(TestClock::new(w.clock.now_ms() - HOUR_MS))
}

#[tokio::test]
async fn test_recovery_clock_gone_back_empty_history_stays_uncertain() {
    let w = World::new();
    let base = store();
    let (attempt_id, app_id) = in_flight(&w, &base).await;
    // A history kept for a day: empty, it shows a dispatch never started only within that
    // day of the journaling, which a clock now behind it can't tell.
    w.rail.set_declaration(RailDeclaration {
        idempotent: false,
        idempotency_retention_ms: None,
        history: true,
        history_retention_ms: Some(DAY_MS),
        history_survives_restore: true,
    });
    w.rail.forget_history(&attempt_id);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), app_id, clock_gone_back(&w))
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    // Nothing was written `failed`: the attempt stays open, "may be paid".
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, Stage::Paying);
    let claim = wallet
        .app_mut()
        .read_claim(&payment.obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Attempting);
    assert_eq!(claim.view.attempts[0].outcome, Outcome::Open);
}

#[tokio::test]
async fn test_recovery_clock_gone_back_idempotent_rail_not_dispatched_again() {
    let w = World::new();
    let base = store();
    let (attempt_id, app_id) = in_flight(&w, &base).await;
    // Idempotent for a day, without a history: a repeat is harmless only while the method
    // still remembers the key, which a clock now behind the journaling can't tell.
    w.rail.set_declaration(RailDeclaration {
        idempotent: true,
        idempotency_retention_ms: Some(DAY_MS),
        history: false,
        history_retention_ms: None,
        history_survives_restore: false,
    });
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), app_id, clock_gone_back(&w))
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_recovery_within_retention_idempotent_rail_asked_again() {
    let w = World::new();
    let base = store();
    let (attempt_id, app_id) = in_flight(&w, &base).await;
    w.rail.set_declaration(RailDeclaration {
        idempotent: true,
        idempotency_retention_ms: Some(DAY_MS),
        history: false,
        history_retention_ms: None,
        history_survives_restore: false,
    });
    // The clock moved on, within the day: the method still remembers the key, and every
    // check before paying passes again.
    w.clock.advance_ms(HOUR_MS);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&base),
            app_id,
            Arc::new(TestClock::new(w.clock.now_ms())),
        )
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Pending { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 2);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_recovery_clock_gone_back_own_identity_read_again() {
    let w = World::new();
    let base = store();
    let wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    drop(wallet);
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let device = Arc::new(TestClock::new(w.clock.now_ms()));
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&device))
        .await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app_mut()
        .refresh_identity()
        .await
        .unwrap()
        .confirmed_at()
        .is_some());
    // The own identity.json goes missing, and the device clock goes back 10 minutes: the
    // confirmation it now puts in the future shows nothing recent (Paykit spec 4).
    w.homeserver
        .remove(w.alice_id(), pubky_switch_core::records::IDENTITY_PATH);
    device.set_ms(device.now_ms() - 10 * 60_000);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::IdentityUnavailable(_))
        ),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

/// An idempotent rail without a history, remembering keys for good: only the claim can tell
/// a resume that an attempt was resolved.
fn idempotent_without_history() -> RailDeclaration {
    RailDeclaration {
        idempotent: true,
        idempotency_retention_ms: None,
        history: false,
        history_retention_ms: None,
        history_survives_restore: false,
    }
}

#[tokio::test]
async fn test_recovery_attempt_resolved_failed_may_be_paid_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    // The user resolves the attempt as failed: the journal follows, so the request is
    // payable again, here as on the claim.
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    wallet
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Failed,
            pubky_switch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert_eq!(wallet.payments().await.unwrap()[0].stage, Stage::Finished);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        paykit_sdk::RequestStatus::Open
    );
    let again = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(again, PayOutcome::Paid { .. }), "{again:?}");
}

#[tokio::test]
async fn test_recovery_attempt_resolved_done_never_dispatched_again() {
    let w = World::new();
    w.rail.set_declaration(idempotent_without_history());
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    wallet
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Done,
            pubky_switch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap();
    // A resume finds nothing to do: the user said it was paid.
    assert!(wallet.resume_payments().await.unwrap().is_empty());
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_recovery_attempt_resolved_on_the_claim_not_dispatched_again() {
    let w = World::new();
    w.rail.set_declaration(idempotent_without_history());
    let base = store();
    let (attempt_id, app_id) = in_flight(&w, &base).await;
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&base),
            app_id,
            Arc::new(TestClock::new(w.clock.now_ms())),
        )
        .await;
    // The attempt is resolved on the claim behind the journal's back, as a receipt or
    // another own app's resolution would: the resume reads the claim before any repeat.
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let step = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap()
        .view
        .step;
    wallet
        .app_mut()
        .write_claim(
            pubky_switch_client::app::ClaimOp::done(
                obligation,
                step,
                attempt_id.to_bytes(),
                pubky_switch_core::claims::Basis::Owner,
                None,
            )
            .unwrap(),
            Vec::new(),
            pubky_switch_client::app::ObligationDeadline::NotKnown,
        )
        .await
        .unwrap();
    assert!(wallet.resume_payments().await.unwrap().is_empty());
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    assert_eq!(wallet.payments().await.unwrap()[0].stage, Stage::Finished);
}

#[tokio::test]
async fn test_recovery_repeat_refused_without_a_fresh_own_identity() {
    let w = World::new();
    w.rail.set_declaration(idempotent_without_history());
    let base = store();
    let (attempt_id, app_id) = in_flight(&w, &base).await;
    // The payer's identity.json goes missing, and its last confirmation is over 60 s old:
    // a repeat may execute money, so it needs the check a first dispatch does (Paykit spec
    // 4, 7.2).
    w.homeserver
        .remove(w.alice_id(), pubky_switch_core::records::IDENTITY_PATH);
    w.clock.advance_ms(61_000);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&base),
            app_id,
            Arc::new(TestClock::new(w.clock.now_ms())),
        )
        .await;
    let refused = wallet.resume_payments().await.unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::IdentityUnavailable(_))
        ),
        "{refused:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    assert_eq!(wallet.payments().await.unwrap()[0].stage, Stage::Paying);
}

#[tokio::test]
async fn test_recovery_repeat_to_a_revoked_payee_app_held() {
    let w = World::new();
    w.rail.set_declaration(idempotent_without_history());
    let base = store();
    let (attempt_id, app_id) = in_flight(&w, &base).await;
    // Bob revokes the shop whose request carries the destination; the payer's copy of his
    // identity.json is looked up again once over 10 minutes old.
    let shop_id = w.bob.identity().app_keys("shop").app_id_bytes();
    let now = w.clock.now_ms();
    w.bob
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != shop_id);
            body.removed.push(pubky_switch_core::records::RemovedApp {
                app_id: shop_id,
                distrust_from: now,
            });
        })
        .unwrap();
    w.clock.advance_ms(11 * 60_000);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&base),
            app_id,
            Arc::new(TestClock::new(w.clock.now_ms())),
        )
        .await;
    // Not repeated, and not written `failed`: the first dispatch may have paid.
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    assert_eq!(wallet.payments().await.unwrap()[0].stage, Stage::Paying);
}

#[tokio::test]
async fn test_recovery_slow_failed_own_identity_read_refuses_the_repeat() {
    let w = World::new();
    w.rail.set_declaration(idempotent_without_history());
    let base = store();
    let (attempt_id, app_id) = in_flight(&w, &base).await;
    let homeserver = Arc::new(crate::common::SlowFailingIdentityRead {
        inner: Arc::clone(&w.homeserver),
        clock: Arc::clone(&w.clock),
        identity: w.alice_id(),
        delay_ms: 61_000,
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let mut wallet = w
        .load_with_homeserver(
            &w.alice,
            Arc::clone(&base),
            app_id,
            Arc::clone(&homeserver) as Arc<dyn pubky_switch_client::app::HomeserverReader>,
        )
        .await;
    // Confirmed now; then the refresh before the repeat takes 61 s and fails. The 60 s are
    // judged on the clock after the read: the confirmation is too old, and nothing is paid
    // (Paykit spec 4).
    assert!(wallet
        .app_mut()
        .refresh_identity()
        .await
        .unwrap()
        .confirmed_at()
        .is_some());
    homeserver.arm();
    let refused = wallet.resume_payments().await.unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::IdentityUnavailable(_))
        ),
        "{refused:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}
