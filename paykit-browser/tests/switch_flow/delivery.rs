//! Where a Payment Request goes when the real service cannot take it yet (Switch spec 11): a
//! request the payer's switch does not admit waits until the payer invites the payee, and a
//! request to an identity that never enrolled stays queued.

use paykit_browser::clock::JsClock;
use paykit_sdk::MessageDelivery;
use pubky_switch_client::clock::Clock;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::{draft, world, DAY_MS, PAYEE, PAYER};

#[wasm_bindgen_test]
async fn test_switch_flow_request_waits_for_the_payer_to_admit_the_payee() {
    // The payer's wallet hasn't invited the payee: the service answers the payee's request
    // `403 not_admitted` (Switch spec 7), and it waits for the payer, not for the shop's
    // certificate. Once the payer invites the payee, it goes.
    let (w, mut wallet, mut shop) = world().await;
    let before = JsClock.now_ms();
    let sent = shop
        .send_request(w.id(PAYER), draft("1", JsClock.now_ms() + DAY_MS))
        .await
        .unwrap();
    let after = JsClock.now_ms();
    assert!(
        matches!(
            sent.delivery,
            MessageDelivery::WaitingForAdmission { since_ms, tries: 1 }
                if (before..=after).contains(&since_ms)
        ),
        "{:?}",
        sent.delivery
    );
    assert_eq!(
        shop.message_delivery(&sent.msg_id).await.unwrap(),
        Some(sent.delivery.clone())
    );
    // Nothing reached the payer's inbox.
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());

    wallet.app_mut().invite(&w.id(PAYEE)).await.unwrap();
    shop.flush().await.unwrap();
    let delivery = shop.message_delivery(&sent.msg_id).await.unwrap();
    assert!(
        matches!(delivery, Some(MessageDelivery::Accepted { .. })),
        "{delivery:?}"
    );
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(summary.applied, [sent.msg_id], "{summary:?}");
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
}

#[wasm_bindgen_test]
async fn test_switch_flow_request_to_an_identity_never_enrolled_stays_queued() {
    // The identity has no `identity.json` on its Homeserver, so the client knows no switch to
    // deliver to (Switch spec 4.3) and sends nothing: the request is recorded at the payee's
    // switch and queued, and the call still reports it sent, so the host doesn't send a
    // second one.
    let (w, _wallet, mut shop) = world().await;
    let sent = shop
        .send_request(w.id("stranger"), draft("1", JsClock.now_ms() + DAY_MS))
        .await
        .unwrap();
    assert!(sent.request.is_some(), "the request is recorded");
    assert!(
        matches!(
            &sent.delivery,
            MessageDelivery::Queued {
                tries: 1,
                last_error: Some(error),
            } if error.contains("Switch spec 4.3")
        ),
        "{:?}",
        sent.delivery
    );
    assert_eq!(shop.payee_activity().await.unwrap().len(), 1);
    assert_eq!(
        shop.message_delivery(&sent.msg_id).await.unwrap(),
        Some(sent.delivery.clone())
    );
}
