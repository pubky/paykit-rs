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

#[tokio::test]
async fn test_resend_time_selector_matches_a_later_resend_acceptance() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    let request = sent.request.unwrap();
    let first_accepted = w.clock.now_ms();
    wallet.receive(None).await.unwrap();

    // The phone joins later and asks for the request by its hash: bob re-sends it, and the
    // re-send is accepted again, an hour and more after the first acceptance.
    w.clock.advance_ms(HOUR_MS + 11 * 60_000);
    let mut phone = w.wallet("phone").await;
    w.clock.advance_ms(11 * 60_000);
    phone
        .request_resend(
            w.bob_id(),
            ResendSelection {
                request_hashes: vec![request.request_hash],
                ..ResendSelection::default()
            },
        )
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let resent_at = w.clock.now_ms();
    assert!(phone
        .receive(None)
        .await
        .unwrap()
        .applied
        .contains(&sent.msg_id));

    // A tablet joins after that, and asks by time for only the re-send's acceptance: the
    // window misses the first acceptance, and matches the later one (Paykit wire 4.12).
    w.clock.advance_ms(HOUR_MS + 1);
    let mut tablet = w.wallet("tablet").await;
    w.clock.advance_ms(11 * 60_000);
    assert!(first_accepted < resent_at - 1);
    tablet
        .request_resend(
            w.bob_id(),
            ResendSelection {
                accepted_from_ms: Some(resent_at - 1),
                accepted_until_ms: Some(resent_at + 1),
                ..ResendSelection::default()
            },
        )
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let got = tablet.receive(None).await.unwrap();
    assert!(got.applied.contains(&sent.msg_id), "{got:?}");
}
