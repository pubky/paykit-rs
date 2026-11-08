//! Re-sends (Paykit spec 9, Paykit wire 4.12): an app that missed a message asks its sender,
//! which re-sends what the request selects, at most once an hour per message.

use paykit_sdk::ResendSelection;
use pubky_switch_client::clock::Clock;

use crate::common::{draft, World, HOUR_MS};

#[tokio::test]
async fn test_resend_request_answered_once_an_hour() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    let request = sent.request.unwrap();
    wallet.receive(None).await.unwrap();

    // The phone joins after the request went out: it was never sealed to it.
    let mut phone = w.wallet("phone").await;
    // Bob's apps read alice's identity.json again once their copy is older than 10 minutes,
    // and then take the phone's messages (Switch spec 4.3).
    w.clock.advance_ms(11 * 60_000);
    let ask = ResendSelection {
        request_hashes: vec![request.request_hash],
        ..ResendSelection::default()
    };
    phone.request_resend(w.bob_id(), ask.clone()).await.unwrap();
    shop.receive(None).await.unwrap();
    let got = phone.receive(None).await.unwrap();
    assert!(got.applied.contains(&sent.msg_id), "{got:?}");

    // Asked again within the hour, the sender re-sends nothing; after it, it does.
    phone.request_resend(w.bob_id(), ask.clone()).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(phone.receive(None).await.unwrap().applied.is_empty());
    w.clock.advance_ms(HOUR_MS + 1);
    phone.request_resend(w.bob_id(), ask).await.unwrap();
    shop.receive(None).await.unwrap();
    let report = phone.receive(None).await.unwrap().report;
    assert!(
        report
            .events
            .iter()
            .any(|e| matches!(e, pubky_switch_client::app::AppEvent::Duplicate { .. })),
        "{:?}",
        report.events
    );
}

#[tokio::test]
async fn test_resend_selecting_nothing_refused() {
    let w = World::new();
    let mut phone = w.wallet("phone").await;
    // Bob's apps read alice's identity.json again once their copy is older than 10 minutes,
    // and then take the phone's messages (Switch spec 4.3).
    w.clock.advance_ms(11 * 60_000);
    let refused = phone
        .request_resend(w.bob_id(), ResendSelection::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused, paykit_sdk::PaykitError::Lib(_)),
        "{refused:?}"
    );
}
