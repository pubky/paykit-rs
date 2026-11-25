//! The payment journal drops finished payments past their claim's reconciliation window,
//! and keeps what a later reconcile needs (Paykit spec 7.1, 7.2; Dispatch spec 6.3).

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Paykit};
use pubky_dispatch_client::clock::Clock;
use pubky_dispatch_core::claims::RECONCILIATION_MS;
use pubky_dispatch_core::records::DAY_MS;

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
        .outcome
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
        .outcome
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

/// The JSON Paykit keeps at `key`, `None` where nothing is.
async fn held(wallet: &Paykit, key: &str) -> Option<serde_json::Value> {
    wallet
        .app()
        .protocol_get("paykit", key.as_bytes())
        .await
        .unwrap()
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
}

/// Put `value` at `key` of Paykit's state.
async fn put(wallet: &mut Paykit, key: &str, value: &serde_json::Value) {
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_dispatch_client::app::LocalWrite::Put {
                key: key.as_bytes().to_vec(),
                value: serde_json::to_vec(value).unwrap(),
            }],
        )
        .await
        .unwrap();
}

/// A finished payment whose facts hold a sighting of it made, `dropped` by the Allowance
/// ledger or not, pruned past its window: returns whether its facts are still kept.
async fn sighted_facts_kept_after_pruning(dropped: bool) -> bool {
    let w = World::new();
    let mut wallet = paid(&w, Behaviour::Succeed).await;
    let payment = wallet.payments().await.unwrap().remove(0);
    let key = format!("jf/{}", payment.attempt_id.to_text());
    let entry = held(&wallet, &format!("j/{}", payment.attempt_id.to_text()))
        .await
        .unwrap();
    let mut facts = held(&wallet, &key).await.unwrap();
    facts["d"]["paid_seen"] = serde_json::json!({
        "source": { "kind": "signed_done" },
        "obligation": entry["d"]["obligation"],
        "first_seen_ms": w.clock.now_ms(),
    });
    facts["d"]["ledger_dropped"] = serde_json::json!(dropped);
    put(&mut wallet, &key, &facts).await;
    later(&w, &mut wallet, DAY_MS + RECONCILIATION_MS + 1_000).await;
    assert!(wallet.payments().await.unwrap().is_empty());
    held(&wallet, &key).await.is_some()
}

#[tokio::test]
async fn test_pruning_keeps_a_sighting_until_the_allowance_ledger_drops_the_attempt() {
    // The facts go with their entry, but a sighting of the payment made stays while the
    // Allowance ledger may still list the attempt, and goes with the entry once it doesn't.
    assert!(sighted_facts_kept_after_pruning(false).await);
    assert!(!sighted_facts_kept_after_pruning(true).await);
}
