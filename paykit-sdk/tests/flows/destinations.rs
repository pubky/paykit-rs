//! Destination checks before paying (Paykit spec 4, 7.2): only from the payee's current app.

use paykit_sdk::{PayOptions, Refusal, RequestStatus};
use pubky_switch_client::clock::Clock;
use pubky_switch_core::claims::Outcome;
use pubky_switch_core::records::RemovedApp;

use crate::common::{draft, request_from, World};

#[tokio::test]
async fn test_destinations_from_a_revoked_app_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();

    // Bob's signer revokes the shop app; its request is still held.
    let shop_id = shop.app_id();
    let now = w.clock.now_ms();
    w.bob
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != shop_id);
            body.removed.push(RemovedApp {
                app_id: shop_id,
                distrust_from: now,
            });
        })
        .unwrap();
    w.clock.advance_ms(1000);

    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Destination(_))),
        "{refused:?}"
    );
    // The attempt was written, then `failed` instead of paying: nothing moved.
    assert!(w.rail.all_executions().is_empty());
    let payments = wallet.payments().await.unwrap();
    assert_eq!(payments.len(), 1);
    let claim = wallet
        .app_mut()
        .read_claim(&payments[0].obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.attempts[0].outcome, Outcome::Failed);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_destinations_from_a_current_app_paid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // Another app of bob's added meanwhile doesn't change the shop's standing.
    let _other = w.labelled_shop("other", "other").await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn test_destinations_payee_identity_unreadable_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // The payee's identity.json can't be read before money moves: the payment fails closed.
    w.homeserver
        .remove(w.bob_id(), pubky_switch_core::records::IDENTITY_PATH);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Destination(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}
