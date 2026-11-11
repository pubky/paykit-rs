//! A blocked payee's requests are paused at a payer app holding `contacts`, and paid only
//! once the user confirms what it was shown (Paykit spec 8.1, "Blocked counterparties").

use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, PaykitConfig, Refusal, RequestRef, RequestStatus,
};
use pubky_switch_client::clock::Clock;

use crate::common::{btc, draft, request_from, store, World};

/// Alice's wallet holding `pay` and `manage_contacts`, with bob's request received, then
/// bob blocked. Returns the wallet and the request.
async fn blocked(w: &World) -> (Paykit, RequestRef) {
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    (wallet, request)
}

/// The payment the refusal shows.
fn shown(error: paykit_sdk::PaykitError) -> paykit_sdk::BlockedPayment {
    match error.refusal() {
        Some(Refusal::Blocked { payment }) => (**payment).clone(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn test_blocked_request_paid_only_once_the_user_confirms_it() {
    let w = World::new();
    let (mut wallet, request) = blocked(&w).await;
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    let payment = shown(error);
    assert_eq!(payment.payee, w.bob_id());
    assert_eq!(payment.request, request);
    assert_eq!(payment.amount, btc("1"));
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Blocked
    );
    // A confirmation of something else passes nothing.
    let mut other = payment.clone();
    other.amount = btc("2");
    let again = wallet
        .pay_request(
            &request,
            PayOptions::default().with_blocked_confirmed(other),
        )
        .await
        .unwrap_err();
    assert_eq!(shown(again), payment);
    let outcome = wallet
        .pay_request(
            &request,
            PayOptions::default().with_blocked_confirmed(payment),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_blocked_pause_ends_when_the_payee_is_unblocked() {
    let w = World::new();
    let (mut wallet, request) = blocked(&w).await;
    wallet.app_mut().undeny(&w.bob_id()).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}
