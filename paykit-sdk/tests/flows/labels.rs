//! Labelled selling scopes (Paykit spec 3.1, 3.4, 6, 8.1): a hosted app holding
//! `paykit_request:<label>` sends every request under its label and gets the payer's replies only
//! about its label's requests; the identity's apps holding `paykit_request` see them too.

use paykit_lib::Label;
use paykit_sdk::{AppScopes, PayOptions, PayOutcome, PayeeStatus, PaykitConfig, Refusal};
use pubky_switch_client::clock::Clock;

use crate::common::{draft, request_from, store, World};

#[tokio::test]
async fn test_labels_reply_reaches_only_the_labelled_app_and_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop1 = w.labelled_shop("shop1", "shop1").await;
    let mut shop2 = w.labelled_shop("shop2", "shop2").await;
    // The identity's own app holding `paykit_request` reads every label's records, with no list of
    // labels to follow.
    let mut owner = w
        .join(&w.bob, "owner", AppScopes::new().request().build().unwrap())
        .await;
    let request = request_from(&mut shop1, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.label.as_deref(), Some("shop1"));
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");

    // The labelled app and the identity's own `request` app get the replies; another
    // label's app gets none.
    shop1.receive(None).await.unwrap();
    let sales = shop1.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].label.as_deref(), Some("shop1"));
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    let other = shop2.receive(None).await.unwrap();
    assert!(other.applied.is_empty(), "{other:?}");
    assert!(shop2.payee_activity().await.unwrap().is_empty());
    let seen = owner.receive(None).await.unwrap();
    assert_eq!(seen.applied.len(), 2, "{seen:?}");
    assert_eq!(owner.payee_activity().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_labels_labelled_app_needs_its_label() {
    let w = World::new();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop1",
            AppScopes::new()
                .request_label(&Label::parse("shop1").unwrap())
                .build()
                .unwrap(),
            PaykitConfig::default().with_selling_label(Label::parse("shop2").unwrap()),
            store(),
        )
        .await;
    let refused = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Label(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_labels_payer_cannot_send_requests() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let refused = wallet
        .send_request(w.bob_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::ScopeMissing(_))),
        "{refused:?}"
    );
}
