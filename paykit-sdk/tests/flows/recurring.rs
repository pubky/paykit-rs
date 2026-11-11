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

/// How many rejections bob's shop receives at its next read.
async fn rejections_received(shop: &mut Paykit) -> usize {
    let got = shop.receive(None).await.unwrap();
    got.report
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                pubky_switch_client::app::AppEvent::MessageReceived { kind, .. }
                    if kind == "paykit.payment_request_rejection"
            )
        })
        .count()
}

#[tokio::test]
async fn test_recurring_request_rejected_at_its_expiry_millisecond() {
    // The expiry is inclusive: its own millisecond still records and sends the rejection.
    let w = World::new();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut wallet = w.wallet("wallet").await;
    let expires_at = w.clock.now_ms() + HOUR_MS;
    let request = monthly_to(&w, &mut wallet, expires_at).await;
    w.clock.advance_ms(expires_at - w.clock.now_ms());
    wallet.reject_request(&request, None).await.unwrap();
    assert_eq!(rejections_received(&mut shop).await, 1);
}

#[tokio::test]
async fn test_recurring_request_rejection_after_its_proposal_expired_refused() {
    // One millisecond past the expiry, nothing is recorded or sent: the refusal says the
    // proposal expired, the same on every try, and the request shows expired.
    let w = World::new();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut wallet = w.wallet("wallet").await;
    let expires_at = w.clock.now_ms() + HOUR_MS;
    let request = monthly_to(&w, &mut wallet, expires_at).await;
    w.clock.advance_ms(expires_at + 1 - w.clock.now_ms());
    for _ in 0..2 {
        let refused = wallet.reject_request(&request, None).await.unwrap_err();
        assert!(
            matches!(refused.refusal(), Some(Refusal::DeadlinePassed)),
            "{refused:?}"
        );
    }
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Expired
    );
    assert_eq!(rejections_received(&mut shop).await, 0);
}

/// What alice's phone, another own app, does to bob's monthly request first.
#[derive(Clone, Copy, Debug)]
enum FirstAnswer {
    /// Accepts it: the phone writes its acceptance into the request record.
    Accept,
    /// Rejects it.
    Reject,
    /// Reads bob's cancellation of it, which the phone places in the record.
    PayeeCancelled,
}

/// The phone's acceptance of `request`, as the request record stores it.
fn phone_acceptance(w: &World, request: &RequestRef) -> paykit_lib::records::StoredMessage {
    let plaintext = w.sign_as(
        &w.alice,
        "phone",
        &w.bob,
        "paykit.payment_request_acceptance",
        vec![request.request_hash],
        serde_json::Map::new(),
    );
    let keys = w.alice.identity().app_keys("phone");
    let cert = w.alice.switch().app(&keys.app_id_bytes()).cert.unwrap();
    paykit_lib::records::StoredMessage::new(
        w.alice_id(),
        cert.signed(),
        plaintext,
        None,
        &w.bob_id(),
    )
    .unwrap()
}

/// Alice's wallet and phone both hold bob's monthly request, open for an hour. The phone
/// answers it first, as `first` says; the clock then passes the proposal expiry, and the
/// wallet, which last heard from its switch before then and still holds the request record
/// as it was, rejects it. Its batch is refused with `409`, which the service answers past
/// the expiry. Returns the wallet's refusal, and the wallet.
async fn rejected_across_expiry(w: &World, first: FirstAnswer) -> (Refusal, Paykit, RequestRef) {
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let expires_at = w.clock.now_ms() + HOUR_MS;
    let request = monthly_to(w, &mut wallet, expires_at).await;
    phone.receive(None).await.unwrap();
    match first {
        FirstAnswer::Accept => {
            let target = crate::outcome_record::payer_record(&request);
            let held = phone.app_mut().read_record(&target).await.unwrap().unwrap();
            let mut record = paykit_lib::records::ActivityRecord::read(
                held.content.value().unwrap(),
                &w.alice_id(),
                paykit_lib::records::ActivitySide::Payer,
            )
            .unwrap();
            record.response = Some(phone_acceptance(w, &request));
            phone
                .app_mut()
                .put_record(&target, &record.to_json().unwrap(), Vec::new())
                .await
                .unwrap();
        }
        FirstAnswer::Reject => {
            phone.reject_request(&request, None).await.unwrap();
        }
        FirstAnswer::PayeeCancelled => {
            w.inject(
                &w.bob,
                "shop",
                &w.alice,
                &[phone.app_id()],
                "paykit.payment_request_cancellation",
                vec![request.request_hash],
                serde_json::Map::new(),
            );
            phone.receive(None).await.unwrap();
            assert_eq!(
                phone.request(&request).await.unwrap().status,
                RequestStatus::Cancelled
            );
        }
    }
    w.clock.advance_ms(expires_at + 1 - w.clock.now_ms());
    let refused = wallet.reject_request(&request, None).await.unwrap_err();
    let refusal = refused
        .refusal()
        .cloned()
        .unwrap_or_else(|| panic!("{refused:?}"));
    (refusal, wallet, request)
}

#[tokio::test]
async fn test_recurring_rejection_crossing_an_acceptance_past_expiry_is_recurring() {
    // Another own app accepted the request before its proposal expired: an accepted
    // recurring request no longer expires as a whole, so the wallet's rejection, refused
    // past the expiry, says the request is accepted and recurring, not that it expired.
    let w = World::new();
    let (refusal, wallet, request) = rejected_across_expiry(&w, FirstAnswer::Accept).await;
    assert_eq!(refusal, Refusal::Recurring);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_recurring_rejection_crossing_another_answer_past_expiry_shows_it() {
    // The request record the service holds says what became of the request: rejected by
    // the phone, or cancelled by bob, though the service answered past the expiry.
    for (first, expected) in [
        (FirstAnswer::Reject, Refusal::Rejected),
        (FirstAnswer::PayeeCancelled, Refusal::Cancelled),
    ] {
        let w = World::new();
        let (refusal, _, _) = rejected_across_expiry(&w, first).await;
        assert_eq!(refusal, expected, "{first:?}");
    }
}

#[tokio::test]
async fn test_recurring_rejection_failing_otherwise_past_expiry_is_the_failure() {
    // The wallet's rejection is answered past the expiry with another error than `409`: the
    // record isn't read again, so whether another own app accepted the request meanwhile
    // isn't known. The failure is the answer, not the expiry.
    let w = World::new();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut wallet = w.wallet("wallet").await;
    let expires_at = w.clock.now_ms() + HOUR_MS;
    let request = monthly_to(&w, &mut wallet, expires_at).await;
    w.clock.advance_ms(expires_at + 1 - w.clock.now_ms());
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::QuotaExceeded,
            current: serde_json::Value::Null,
        });
    let failed = wallet.reject_request(&request, None).await.unwrap_err();
    assert_eq!(
        failed.code(),
        Some(pubky_switch_core::http::ErrorCode::QuotaExceeded),
        "{failed:?}"
    );
}
