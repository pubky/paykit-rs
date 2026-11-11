//! Recurring requests in this part (Paykit spec 8.2): received and shown, never paid, and
//! rejected before acceptance, so a wallet that doesn't pay them can dismiss one.

use paykit_sdk::{PayOptions, Paykit, Refusal, RequestRef, RequestStatus};
use pubky_switch_client::clock::Clock;
use serde_json::json;

use crate::common::{World, HOUR_MS};

/// Bob's shop sends alice's `wallet` a monthly request, entered at her switch directly,
/// with its proposal open until `expires_at`. Returns it as the wallet holds it.
async fn monthly_to(w: &World, wallet: &mut Paykit, expires_at: u64) -> RequestRef {
    let body = json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "plan-1",
        "proposal_expires_at": pubky_switch_core::time::rfc3339(expires_at),
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" },
        "recurrence": {
            "anchor": "2026-01-31T00:00:00.000Z",
            "ends_at": null,
            "every": 1,
            "starts_at": "2026-01-31T00:00:00.000Z",
            "unit": "month"
        }
    });
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_request",
        Vec::new(),
        body.as_object().cloned().unwrap(),
    );
    let summary = wallet.receive(None).await.unwrap();
    let held = wallet.requests().await.unwrap();
    assert_eq!(held.len(), 1, "{summary:?}");
    held[0].request
}

#[tokio::test]
async fn test_recurring_request_rejected_before_acceptance() {
    let w = World::new();
    // Bob's shop signs the request.
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = monthly_to(&w, &mut wallet, w.clock.now_ms() + 24 * HOUR_MS).await;
    // This part pays no recurring request.
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Recurring)),
        "{refused:?}"
    );
    // Its payer cancellation belongs to the recurring flows.
    let refused = wallet.cancel_request(&request, None).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Recurring)),
        "{refused:?}"
    );
    wallet.reject_request(&request, None).await.unwrap();
    let again = wallet.reject_request(&request, None).await.unwrap_err();
    assert!(
        matches!(again.refusal(), Some(Refusal::Rejected)),
        "{again:?}"
    );
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Rejected
    );
}

#[tokio::test]
async fn test_recurring_request_rejection_after_its_proposal_expired_refused() {
    // Conditioned on the proposal's expiry, as an acceptance is: past it, nothing is
    // recorded or sent.
    let w = World::new();
    // Bob's shop signs the request.
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let request = monthly_to(&w, &mut wallet, w.clock.now_ms() + HOUR_MS).await;
    w.clock.advance_ms(2 * HOUR_MS);
    let refused = wallet.reject_request(&request, None).await.unwrap_err();
    assert_ne!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Rejected,
        "{refused:?}"
    );
}
