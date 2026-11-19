//! One-time Payment Requests (Paykit spec 8.1): sent by the payee's shop, received by the
//! payer's wallet, and shown open, expired once their proposal expiry has passed, or
//! rejected; and a request that waits until the payer admits the payee.

use paykit_sdk::{AppScopes, MessageDelivery, PayeeStatus, Refusal, RequestStatus};
use pubky_switch_client::clock::Clock;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::world::{draft, send, stored_request, World, HOUR_MS};

#[wasm_bindgen_test]
async fn test_request_flow_one_time_request_received_open() {
    let w = World::new();
    let mut wallet = w.wallet().await;
    let mut shop = w.shop().await;
    let sent_draft = draft("0.00025", w.clock.now_ms() + 24 * HOUR_MS);
    let sent = shop
        .send_request(w.payer_id(), sent_draft.clone())
        .await
        .unwrap();
    let request = sent.request.expect("the request is accepted at once");

    // The payer's wallet receives the request, once, and shows it open, as the payee sent it.
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(summary.applied, [sent.msg_id], "{summary:?}");
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Open);
    assert_eq!(view.request, request);
    assert_eq!(view.amount, sent_draft.amount);
    assert_eq!(
        view.payment_reference,
        sent_draft.payment_reference.as_str()
    );
    assert_eq!(
        view.proposal_expires_at_ms,
        sent_draft.proposal_expires_at_ms
    );
    assert!(!view.recurring);

    // Its activity record stores the payee's signed request, Payment Endpoint included.
    let stored = stored_request(&wallet, &view).await;
    assert_eq!(stored.amount, sent_draft.amount);
    assert_eq!(stored.destination, sent_draft.destination);
    assert_eq!(
        stored.proposal_expires_at,
        sent_draft.proposal_expires_at_ms
    );
}

#[wasm_bindgen_test]
async fn test_request_flow_expired_request_shown_expired() {
    let w = World::new();
    let mut wallet = w.wallet().await;
    let mut shop = w.shop().await;
    let request = send(
        &mut shop,
        w.payer_id(),
        draft("1", w.clock.now_ms() + HOUR_MS),
    )
    .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );

    // Deadlines are read on the service's clock (Paykit spec 7.1): the wallet learns its time
    // from the next answer it verifies.
    w.clock.advance_ms(2 * HOUR_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Expired
    );
}

#[wasm_bindgen_test]
async fn test_request_flow_rejected_request_refused_again() {
    let w = World::new();
    let mut wallet = w.wallet().await;
    let mut shop = w.shop().await;
    let request = send(
        &mut shop,
        w.payer_id(),
        draft("1", w.clock.now_ms() + 24 * HOUR_MS),
    )
    .await;
    wallet.receive(None).await.unwrap();

    wallet
        .reject_request(&request, Some("user_rejected".into()))
        .await
        .unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Rejected
    );
    // A rejected request isn't rejected twice.
    let again = wallet.reject_request(&request, None).await.unwrap_err();
    assert_eq!(again.refusal(), Some(&Refusal::Rejected));

    // The payee's shop receives the rejection.
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Rejected
    );
}

#[wasm_bindgen_test]
async fn test_request_flow_request_waits_for_the_payer_to_admit_the_payee() {
    // The payer's wallet hasn't invited the payee: the payer's switch doesn't admit the
    // payee's request, which waits for the payer. Once the payer invites the payee, it goes.
    let w = World::new();
    let mut wallet = w
        .join(&w.payer, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w.shop().await;
    let sent = shop
        .send_request(w.payer_id(), draft("1", w.clock.now_ms() + 24 * HOUR_MS))
        .await
        .unwrap();
    let since = w.clock.now_ms();
    assert!(
        matches!(
            sent.delivery,
            MessageDelivery::WaitingForAdmission { since_ms, tries: 1 } if since_ms == since
        ),
        "{:?}",
        sent.delivery
    );

    wallet.app_mut().invite(&w.payee_id()).await.unwrap();
    shop.flush().await.unwrap();
    assert!(
        matches!(
            shop.message_delivery(&sent.msg_id).await.unwrap(),
            Some(MessageDelivery::Accepted { .. })
        ),
        "{:?}",
        shop.message_delivery(&sent.msg_id).await
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
}
