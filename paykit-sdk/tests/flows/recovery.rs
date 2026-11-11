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
    let mut wallet = w
        .wallet_with("wallet", Arc::clone(base), crate::common::day_window())
        .await;
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
        longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
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
        longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
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
        longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
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
        longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
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
    let homeserver = Arc::new(crate::common::ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
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
    homeserver.then(w.alice_id(), 61_000, true);
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

/// A world where alice's wallet stopped right after journaling `Paying`, before the payment
/// method was called, on an idempotent rail without a history. Returns the world, the
/// attempt, the wallet's App ID and its store, as committed.
async fn stopped_before_the_rail() -> (
    World,
    paykit_lib::AttemptId,
    [u8; 16],
    Arc<dyn pubky_switch_client::store::StateStore>,
) {
    use pubky_switch_client::store::{Fault, FaultStore, StateStore};
    for n in 1..64 {
        let w = World::new();
        w.rail.set_declaration(idempotent_without_history());
        let base = store();
        let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
        let app_id = wallet.app_id();
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        drop(wallet);
        let faulty: Arc<dyn StateStore> =
            Arc::new(FaultStore::new(Arc::clone(&base), Fault::StopAfter(n)));
        let mut crashing = w
            .load_on_clock(&w.alice, faulty, app_id, Arc::clone(&w.clock))
            .await;
        let _ = crashing.pay_request(&request, PayOptions::default()).await;
        drop(crashing);
        if !w.rail.all_executions().is_empty() {
            panic!("the rail was reached before the stop at {n}");
        }
        let wallet = w
            .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&w.clock))
            .await;
        let payments = wallet.payments().await.unwrap();
        if let [payment] = payments.as_slice() {
            if payment.stage == Stage::Paying {
                let attempt_id = payment.attempt_id;
                drop(wallet);
                return (w, attempt_id, app_id, base);
            }
        }
    }
    panic!("no commit journals Paying");
}

/// Alice's signer publishes her next identity.json at `generation`, naming `inbox`, on her
/// Homeserver only: the switch her wallet follows isn't told.
fn publish_provider(w: &World, generation: u64, inbox: pubky_switch_core::records::Inbox) {
    let held = w.homeserver.identity_doc(&w.alice_id()).unwrap();
    let old = held.body().inbox.clone();
    let old_generation = held.generation();
    w.alice
        .sign_next(|body| {
            body.service_keys
                .push(pubky_switch_core::records::ServiceKeyEntry {
                    generation: old_generation,
                    key: old.service_key,
                });
            body.generation = generation;
            body.inbox = inbox;
        })
        .unwrap();
}

async fn resume_after_provider_change(change: impl FnOnce(&World)) {
    let (w, attempt_id, app_id, base) = stopped_before_the_rail().await;
    change(&w);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&w.clock))
        .await;
    // The attempt was written at a service or generation the identity no longer names:
    // nothing is paid on it, and it stays "may be paid" until the move rules resolve it
    // (Paykit spec 4, 7.2; Switch spec 12.5, 12.6).
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(wallet.payments().await.unwrap()[0].stage, Stage::Paying);
}

#[tokio::test]
async fn test_recovery_not_paid_after_a_provider_change() {
    resume_after_provider_change(|w| {
        let held = w.homeserver.identity_doc(&w.alice_id()).unwrap();
        publish_provider(
            w,
            held.generation() + 1,
            pubky_switch_core::records::Inbox::new("https://alice-next.example", w.bob_id()),
        );
    })
    .await;
}

#[tokio::test]
async fn test_recovery_not_paid_after_a_generation_change_at_the_same_service() {
    resume_after_provider_change(|w| {
        let held = w.homeserver.identity_doc(&w.alice_id()).unwrap();
        publish_provider(w, held.generation() + 1, held.body().inbox.clone());
    })
    .await;
}

#[tokio::test]
async fn test_recovery_stopped_before_the_rail_paid_at_the_same_service() {
    let (w, attempt_id, app_id, base) = stopped_before_the_rail().await;
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&w.clock))
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_recovery_not_paid_once_the_own_app_is_unlisted() {
    let (w, attempt_id, app_id, base) = stopped_before_the_rail().await;
    // The signer drops the wallet from the identity's apps: its identity.json no longer
    // authorizes it (Paykit spec 4).
    w.alice
        .sign_next(|body| body.apps.retain(|a| a.app_id != app_id))
        .unwrap();
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&w.clock))
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
}

#[tokio::test]
async fn test_recovery_failure_that_completed_after_all_sends_its_proof() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Fail);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let PayOutcome::Failed { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    // The method reported a failure, but the payment completed after all: its history shows
    // it paid. The owner sends its proof (Paykit spec 7.2).
    w.rail.complete_after_failure(&attempt_id);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [PayOutcome::Paid {
                proof_msg_id: Some(_),
                ..
            }]
        ),
        "{resumed:?}"
    );
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        paykit_sdk::RequestStatus::Paid
    );
    // Asked once: it isn't reported twice.
    assert!(wallet.resume_payments().await.unwrap().is_empty());
    // The payee verifies it and its receipt settles the claim.
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].attempt_id, Some(attempt_id));
    assert!(shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    wallet.receive(None).await.unwrap();
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let claim = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Settled);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_recovery_failure_past_the_history_window_not_asked_again() {
    let w = World::new();
    w.rail.set_declaration(RailDeclaration {
        idempotent: true,
        idempotency_retention_ms: None,
        history: true,
        history_retention_ms: Some(DAY_MS),
        history_survives_restore: true,
        longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
    });
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Fail);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let PayOutcome::Failed { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    // Past the day the history keeps a dispatch, it is no evidence either way.
    w.clock.advance_ms(2 * DAY_MS);
    w.rail.complete_after_failure(&attempt_id);
    assert!(wallet.resume_payments().await.unwrap().is_empty());
    assert_ne!(
        wallet.request(&request).await.unwrap().status,
        paykit_sdk::RequestStatus::Paid
    );
}

#[tokio::test]
async fn test_recovery_list_expiring_during_the_own_identity_read_not_paid() {
    let w = World::new();
    w.rail.set_declaration(idempotent_without_history());
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    let mut shop = w.shop().await;
    // Bob's list expires in 30 s.
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-lightning-bolt12"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new("lno1reusable").unwrap(),
            single_use: false,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: w.clock.now_ms(),
        payment_endpoints: endpoints,
        expires_at: w.clock.now_ms() + 30_000,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[app_id],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment {
            payee: w.bob_id(),
            amount: crate::common::btc("0.001"),
            options: PayOptions::default(),
        })
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    drop(wallet);
    // On resume, the list is still usable at the destination check; the own identity.json's
    // read then takes 61 s and succeeds, past the list's expiry. Nothing is paid on it
    // (Paykit spec 5, 7.2).
    let reads = Arc::new(crate::common::ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    reads.then(w.alice_id(), 61_000, false);
    let mut wallet = w
        .load_with_homeserver(
            &w.alice,
            Arc::clone(&base),
            app_id,
            Arc::clone(&reads) as Arc<dyn pubky_switch_client::app::HomeserverReader>,
        )
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

/// The retention the boundary tests declare.
const RETENTION_MS: u64 = HOUR_MS;

/// The ages around the retention's end the boundary tests resume at: one before, at, and
/// one after.
const AROUND_THE_END: [(u64, bool); 3] = [
    (RETENTION_MS - 1, true),
    (RETENTION_MS, false),
    (RETENTION_MS + 1, false),
];

#[tokio::test]
async fn test_recovery_empty_history_proves_nothing_from_the_retention_end() {
    for (age, kept) in AROUND_THE_END {
        let w = World::new();
        let base = store();
        let journaled = w.clock.now_ms();
        let (attempt_id, app_id) = in_flight(&w, &base).await;
        w.rail.set_declaration(RailDeclaration {
            idempotent: false,
            idempotency_retention_ms: None,
            history: true,
            history_retention_ms: Some(RETENTION_MS),
            history_survives_restore: true,
            longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
        });
        w.rail.forget_history(&attempt_id);
        let mut wallet = w
            .load_on_clock(
                &w.alice,
                Arc::clone(&base),
                app_id,
                Arc::new(TestClock::new(journaled + age)),
            )
            .await;
        let resumed = wallet.resume_payments().await.unwrap();
        // Within the retention an empty history shows the dispatch never started; from its
        // end on, the history may have dropped it (Paykit spec 7.2).
        if kept {
            assert!(
                matches!(resumed.as_slice(), [PayOutcome::Failed { .. }]),
                "age {age}: {resumed:?}"
            );
        } else {
            assert_eq!(
                resumed,
                vec![PayOutcome::Uncertain { attempt_id }],
                "age {age}"
            );
        }
    }
}

#[tokio::test]
async fn test_recovery_repeat_only_before_the_idempotency_retention_end() {
    for (age, kept) in AROUND_THE_END {
        let w = World::new();
        let base = store();
        let journaled = w.clock.now_ms();
        let (attempt_id, app_id) = in_flight(&w, &base).await;
        w.rail.set_declaration(RailDeclaration {
            idempotent: true,
            idempotency_retention_ms: Some(RETENTION_MS),
            history: false,
            history_retention_ms: None,
            history_survives_restore: false,
            longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
        });
        w.clock.set_ms(journaled + age);
        let mut wallet = w
            .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&w.clock))
            .await;
        let resumed = wallet.resume_payments().await.unwrap();
        // At the retention's end the method may have forgotten the key: no repeat.
        if kept {
            assert_eq!(
                resumed,
                vec![PayOutcome::Pending { attempt_id }],
                "age {age}"
            );
            assert_eq!(w.rail.pay_calls(&attempt_id), 2, "age {age}");
        } else {
            assert_eq!(
                resumed,
                vec![PayOutcome::Uncertain { attempt_id }],
                "age {age}"
            );
            assert_eq!(w.rail.pay_calls(&attempt_id), 1, "age {age}");
        }
    }
}

#[tokio::test]
async fn test_recovery_failure_asked_about_only_before_the_history_retention_end() {
    for (age, kept) in AROUND_THE_END {
        let w = World::new();
        let base = store();
        let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
        let app_id = wallet.app_id();
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        w.rail.set_declaration(RailDeclaration {
            idempotent: true,
            idempotency_retention_ms: None,
            history: true,
            history_retention_ms: Some(RETENTION_MS),
            history_survives_restore: true,
            longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
        });
        let journaled = w.clock.now_ms();
        w.rail.next(Behaviour::Fail);
        let outcome = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap();
        let PayOutcome::Failed { attempt_id, .. } = outcome else {
            panic!("{outcome:?}");
        };
        drop(wallet);
        w.rail.complete_after_failure(&attempt_id);
        let mut wallet = w
            .load_on_clock(
                &w.alice,
                Arc::clone(&base),
                app_id,
                Arc::new(TestClock::new(journaled + age)),
            )
            .await;
        let resumed = wallet.resume_payments().await.unwrap();
        if kept {
            assert!(
                matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
                "age {age}: {resumed:?}"
            );
        } else {
            assert!(resumed.is_empty(), "age {age}: {resumed:?}");
        }
    }
}
