//! Recurring requests (Paykit spec 8.2): received and shown, accepted or rejected before
//! their proposal expires, never both, and not yet paid by this SDK.

use paykit_sdk::{PayOptions, Paykit, Refusal, RequestRef, RequestStatus};
use pubky_switch_client::clock::Clock;
use serde_json::json;

use crate::common::{World, HOUR_MS};

/// Bob's shop sends alice's `wallet` a monthly request, entered at her switch directly,
/// with its proposal open until `expires_at`. Returns it as the wallet holds it.
async fn monthly_to(w: &World, wallet: &mut Paykit, expires_at: u64) -> RequestRef {
    monthly(w, &[wallet.app_id()], expires_at, "plan-1");
    let summary = wallet.receive(None).await.unwrap();
    let held = wallet.requests().await.unwrap();
    assert_eq!(held.len(), 1, "{summary:?}");
    held[0].request
}

/// Bob's shop sends alice's apps `targets` a monthly request with `reference`, entered at
/// her switch directly, its proposal open until `expires_at`.
fn monthly(w: &World, targets: &[[u8; 16]], expires_at: u64, reference: &str) {
    let body = json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": reference,
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
        targets,
        "paykit.payment_request",
        Vec::new(),
        body.as_object().cloned().unwrap(),
    );
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
    // Its payer cancellation belongs to the flows that pay its periods.
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
/// as it was, rejects it. With `heard`, the wallet has heard from its switch since the
/// expiry, so it sees it passed before it sends anything; otherwise its batch is refused with
/// `409`, which the service answers past the expiry. Returns the wallet's refusal, and the
/// wallet.
async fn rejected_across_expiry(
    w: &World,
    first: FirstAnswer,
    heard: bool,
) -> (Refusal, Paykit, RequestRef) {
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
    if heard {
        wallet.app_mut().read_claim(&[0; 32]).await.unwrap();
    }
    let refused = wallet.reject_request(&request, None).await.unwrap_err();
    let refusal = refused
        .refusal()
        .cloned()
        .unwrap_or_else(|| panic!("{refused:?}"));
    (refusal, wallet, request)
}

#[tokio::test]
async fn test_recurring_rejection_crossing_an_acceptance_past_expiry_is_subscribed() {
    // Another own app accepted the request before its proposal expired: an accepted
    // recurring request no longer expires as a whole, so the wallet's rejection, refused
    // past the expiry, says the subscription is accepted, not that it expired.
    let w = World::new();
    let (refusal, wallet, request) = rejected_across_expiry(&w, FirstAnswer::Accept, false).await;
    assert_eq!(refusal, Refusal::Subscribed);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Subscribed
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
        let (refusal, _, _) = rejected_across_expiry(&w, first, false).await;
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

#[tokio::test]
async fn test_recurring_rejection_past_expiry_seen_first_shows_what_another_app_did() {
    // The wallet has heard from its switch past the expiry, and still holds the request
    // record as it was before the phone answered it: the record read again decides, before
    // anything is sent.
    for (first, expected) in [
        (FirstAnswer::Accept, Refusal::Subscribed),
        (FirstAnswer::Reject, Refusal::Rejected),
        (FirstAnswer::PayeeCancelled, Refusal::Cancelled),
    ] {
        let w = World::new();
        let (refusal, _, _) = rejected_across_expiry(&w, first, true).await;
        assert_eq!(refusal, expected, "{first:?}");
    }
}

/// What alice's phone writes at her payer record of the request, which her wallet holds as
/// it was.
#[derive(Clone, Copy, Debug)]
enum PhoneWrite {
    /// It deletes the record.
    Delete,
    /// A value that doesn't read as an activity record.
    Invalid,
    /// The record of another request, bound to that request's key.
    AnotherRequest,
    /// Nothing; the switch then answers the record's read `404`, the wallet keeping its
    /// copy.
    NotFoundAtTheSwitch,
}

/// Alice's wallet and phone hold bob's monthly request, open for an hour; the phone writes
/// its record as `write` says, and the clock passes the expiry. The wallet, holding the
/// record as it was, rejects it: having heard from its switch since the expiry with `heard`,
/// otherwise with its batch refused with `409` past the expiry. Returns the wallet's error.
async fn rejected_over(w: &World, write: PhoneWrite, heard: bool) -> paykit_sdk::PaykitError {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let store = Arc::new(crate::outcome_record::NoSeenMarks::default());
    let mut wallet = w
        .wallet_on(
            "wallet",
            Arc::clone(&store) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut phone = w.wallet("phone").await;
    let expires_at = w.clock.now_ms() + HOUR_MS;
    let apps = [wallet.app_id(), phone.app_id()];
    monthly(w, &apps, expires_at, "plan-1");
    monthly(w, &apps, expires_at, "plan-2");
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let held = wallet.requests().await.unwrap();
    assert_eq!(held.len(), 2);
    let (request, other) = if held[0].payment_reference == "plan-1" {
        (held[0].request, held[1].request)
    } else {
        (held[1].request, held[0].request)
    };
    // The wallet keeps no mark of what it saw: a later value is taken as it is.
    store.armed.store(true, Ordering::SeqCst);
    let value = match write {
        PhoneWrite::Delete | PhoneWrite::NotFoundAtTheSwitch => None,
        PhoneWrite::Invalid => Some(crate::outcome_record::INVALID.to_vec()),
        PhoneWrite::AnotherRequest => {
            let target = crate::outcome_record::payer_record(&other);
            let held = phone.app_mut().read_record(&target).await.unwrap().unwrap();
            Some(held.content.value().unwrap().to_vec())
        }
    };
    if let PhoneWrite::NotFoundAtTheSwitch = write {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/records/".into(),
                code: pubky_switch_core::http::ErrorCode::NotFound,
                current: serde_json::Value::Null,
            });
    } else {
        crate::outcome_record::phone_writes(&mut phone, &request, value.as_deref()).await;
    }
    w.clock.advance_ms(expires_at + 1 - w.clock.now_ms());
    if heard {
        wallet.app_mut().read_claim(&[0; 32]).await.unwrap();
    }
    wallet.reject_request(&request, None).await.unwrap_err()
}

#[tokio::test]
async fn test_recurring_rejection_past_expiry_over_a_record_gone_or_unreadable() {
    // The record read again isn't there, the switch answering `404` over the wallet's copy
    // included, doesn't read, or holds another request: none of them shows this request
    // unanswered, so none is refused as expired. Seen before
    // anything is sent, a record gone is not found, and one that doesn't read as this
    // request a conflict; after a `409`, the conflict is the answer.
    for (write, heard, gone) in [
        (PhoneWrite::Delete, true, true),
        (PhoneWrite::NotFoundAtTheSwitch, true, true),
        (PhoneWrite::Invalid, true, false),
        (PhoneWrite::AnotherRequest, true, false),
        (PhoneWrite::Delete, false, false),
        (PhoneWrite::Invalid, false, false),
        (PhoneWrite::AnotherRequest, false, false),
    ] {
        let w = World::new();
        let error = rejected_over(&w, write, heard).await;
        if gone {
            assert_eq!(
                error.refusal(),
                Some(&Refusal::NotFound),
                "{write:?}: {error:?}"
            );
        } else {
            assert!(
                matches!(error, paykit_sdk::PaykitError::Conflict(_)),
                "{write:?}, heard {heard}: {error:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_recurring_reassociation_built_on_an_attempted_period_flagged() {
    // The phone attempted a Billing Period early, which the wallet saw. Its next write,
    // built on that version, moves the request to another Allowance from that period: the
    // wallet, holding the version the write builds on, refuses it and keeps what it saw
    // (Paykit wire 5.3 "Seen Allowance").
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    monthly(
        &w,
        &[wallet.app_id(), phone.app_id()],
        w.clock.now_ms() + HOUR_MS,
        "plan-1",
    );
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let request = wallet.requests().await.unwrap()[0].request;
    let target = crate::outcome_record::payer_record(&request);
    let held = phone.app_mut().read_record(&target).await.unwrap().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().unwrap()).unwrap();
    let period = 1_772_236_800u64;
    value["allowance_id"] = json!("d0efffb7-191b-4709-b6c7-e343af5500b4");
    value["attempted_periods"] = json!([period]);
    let attempted = serde_json::to_vec(&value).unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, Some(&attempted)).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().flagged_writes().await.unwrap().is_empty());
    value["reassociations"] = json!([{
        "allowance_id": "55de1f16-1a9e-4504-bc9c-57efc3a2b38c",
        "from_period": period,
    }]);
    let moved = serde_json::to_vec(&value).unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, Some(&moved)).await;
    wallet.receive(None).await.unwrap();
    let flagged = wallet.app().flagged_writes().await.unwrap();
    assert_eq!(flagged.len(), 1, "{flagged:?}");
    let kept = wallet
        .app()
        .shared_record(&wallet.app().record_key(&target).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.content.value(), Some(attempted.as_slice()));
}

#[tokio::test]
async fn test_recurring_reassociation_built_on_a_version_skipped_taken() {
    // The wallet saw the phone attempt a Billing Period early. Before the wallet reads
    // again, the phone writes the attempt over, the period dropped, and then, built on that
    // version, moves the request to another Allowance from that period. The wallet never
    // took the version the move builds on, so that version's reassociation rule isn't its
    // to check: the move is taken (Paykit wire 5.3 "Seen Allowance").
    let w = World::new();
    let _shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    monthly(
        &w,
        &[wallet.app_id(), phone.app_id()],
        w.clock.now_ms() + HOUR_MS,
        "plan-1",
    );
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let request = wallet.requests().await.unwrap()[0].request;
    let target = crate::outcome_record::payer_record(&request);
    let held = phone.app_mut().read_record(&target).await.unwrap().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().unwrap()).unwrap();
    let period = 1_772_236_800u64;
    value["allowance_id"] = json!("d0efffb7-191b-4709-b6c7-e343af5500b4");
    value["attempted_periods"] = json!([period]);
    let attempted = serde_json::to_vec(&value).unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, Some(&attempted)).await;
    wallet.receive(None).await.unwrap();
    value.as_object_mut().unwrap().remove("attempted_periods");
    let over = serde_json::to_vec(&value).unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, Some(&over)).await;
    value["reassociations"] = json!([{
        "allowance_id": "55de1f16-1a9e-4504-bc9c-57efc3a2b38c",
        "from_period": period,
    }]);
    let moved = serde_json::to_vec(&value).unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, Some(&moved)).await;
    wallet.receive(None).await.unwrap();
    let flagged = wallet.app().flagged_writes().await.unwrap();
    assert!(flagged.is_empty(), "{flagged:?}");
    let kept = wallet
        .app()
        .shared_record(&wallet.app().record_key(&target).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.content.value(), Some(moved.as_slice()));
}
