//! The payment journal drops finished payments past their claim's reconciliation window,
//! and keeps what a later reconcile needs (Paykit spec 7.1, 7.2; Switch spec 6.3).

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Paykit};
use pubky_switch_client::clock::Clock;
use pubky_switch_core::claims::RECONCILIATION_MS;
use pubky_switch_core::records::DAY_MS;

use crate::common::{draft, request_from, World};

/// Alice's wallet pays bob's request, which expires a day from now, with the rail doing
/// `behaviour`. Returns the wallet.
async fn paid(w: &World, behaviour: Behaviour) -> Paykit {
    let hang = matches!(behaviour, Behaviour::Hang);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(behaviour);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, PayOutcome::Paid { .. } | PayOutcome::Failed { .. })
            || hang && matches!(outcome, PayOutcome::Pending { .. }),
        "{outcome:?}"
    );
    wallet
}

/// The clock moves `ms` on, the wallet verifies a service time there and resumes.
async fn later(w: &World, wallet: &mut Paykit, ms: u64) {
    w.clock.advance_ms(ms);
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
}

#[tokio::test]
async fn test_pruning_drops_a_finished_payment_past_its_window() {
    let w = World::new();
    let mut wallet = paid(&w, Behaviour::Succeed).await;
    later(&w, &mut wallet, DAY_MS + RECONCILIATION_MS + 1_000).await;
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_pruning_keeps_a_finished_payment_within_its_window() {
    let w = World::new();
    let mut wallet = paid(&w, Behaviour::Succeed).await;
    later(&w, &mut wallet, DAY_MS + RECONCILIATION_MS - 60_000).await;
    assert_eq!(wallet.payments().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_pruning_keeps_a_failure_the_method_may_still_show_paid() {
    let w = World::new();
    let mut wallet = paid(&w, Behaviour::Fail).await;
    later(&w, &mut wallet, DAY_MS + RECONCILIATION_MS + 1_000).await;
    assert_eq!(wallet.payments().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_pruning_keeps_a_payment_without_deadline_past_30_days() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut unexpiring = draft("1", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let request = request_from(&mut shop, w.alice_id(), unexpiring).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // Its claim is pinned to none: kept 400 days after its last entry.
    later(&w, &mut wallet, DAY_MS + RECONCILIATION_MS + 1_000).await;
    assert_eq!(wallet.payments().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_pruning_keeps_a_payment_still_open_past_its_window() {
    let w = World::new();
    let mut wallet = paid(&w, Behaviour::Hang).await;
    later(&w, &mut wallet, DAY_MS + RECONCILIATION_MS + 1_000).await;
    let payments = wallet.payments().await.unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].stage, paykit_sdk::Stage::Paying);
}
