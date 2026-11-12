//! Entries the client stores for good, as Paykit handles them (Switch spec 8.2): an effect
//! the switch refuses is written later by the app, an entry dropped unverified is asked for
//! again, and the SDK acknowledges the duty events it took.

use std::sync::Arc;

use paykit_sdk::{AppScopes, PayOptions, PayeeStatus, Paykit, PaykitConfig, Rail, RequestStatus};
use pubky_switch_client::app::AppEvent;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::{Fault, TestSigner};
use pubky_switch_core::claims::Outcome;
use pubky_switch_core::http::ErrorCode;

use crate::common::{
    asked, draft, mark_senders, refuse_writes, request_from, seed_marks, store, World,
};
use pubky_switch_core::records::DAY_MS;

/// Whether `events` hold a duty event about an entry stored locally.
fn about_stored(events: &[AppEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            AppEvent::Parked { id: Some(_), .. }
                | AppEvent::EffectRefused { .. }
                | AppEvent::EffectUncertain { .. }
                | AppEvent::StoredEntryDropped { .. }
        )
    })
}

#[tokio::test]
async fn test_stored_refused_request_shown_unrecorded_then_written() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    // The receive step's write is refused for good, and so is the app's first write of it.
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Unrecorded
    );
    assert_eq!(
        wallet.requests().await.unwrap()[0].status,
        RequestStatus::Unrecorded
    );
    // Not paid while unrecorded.
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    // The SDK took the duty events and acknowledged them.
    let events = wallet.app_mut().unacknowledged_events().await.unwrap();
    assert!(!about_stored(&events), "{events:?}");
    // Room frees: the next receive writes the record, and the stored entry goes.
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    // Written, its note goes; the marks of the events taken go once they are acknowledged.
    assert!(kept(&wallet, "se/").await.is_empty());
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "dt/").await.is_empty());
}

/// The keys of Paykit's local state under `prefix`.
async fn kept(app: &paykit_sdk::Paykit, prefix: &str) -> Vec<Vec<u8>> {
    app.app()
        .protocol_scan(prefix.as_bytes())
        .await
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

#[tokio::test]
async fn test_stored_refused_request_written_later_notes_its_sender() {
    // A request applied at its first read notes bob as a counterparty to ask for re-sends
    // after a `missed` item (Paykit spec 9); one written later does too, once written.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "peer/").await.is_empty());
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "se/").await.is_empty());
    assert_eq!(kept(&wallet, "peer/").await.len(), 1);
}

#[tokio::test]
async fn test_stored_refused_list_held_notes_its_sender() {
    // A list applied at its first read notes bob as a counterparty, its hold included; one
    // whose step is refused is held, and notes him too.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    assert_eq!(kept(&wallet, "peer/").await.len(), 1);
}

#[tokio::test]
async fn test_stored_refused_proof_at_the_payee_written_later() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    // The acceptance's and the proof's steps are refused, and the app's first writes too.
    refuse_writes(&w.bob, 4);
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Requested
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived
    );
    assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
}

/// Bob's request to alice, stored unverified at her wallet while bob's standing can't be
/// checked, then dropped past 21 days, with the homeserver down. Returns the request and
/// when alice's inbox accepted it.
async fn dropped_unverified(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
) -> (paykit_sdk::RequestRef, u64) {
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut unexpiring = draft("1", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let request = request_from(shop, w.alice_id(), unexpiring).await;
    w.homeserver.set_down(true);
    // Past the 10 minutes bob's `identity.json` is cached for.
    w.clock.advance_ms(11 * 60_000);
    let _ = wallet.receive(None).await;
    w.clock
        .advance_ms(pubky_switch_client::app::DEFAULT_PARK_AFTER_MS + 1);
    let _ = wallet.receive(None).await;
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
    w.clock
        .advance_ms(pubky_switch_client::app::MAX_UNVERIFIED_AGE_MS);
    // The bounds are judged on the service time verified: the first read verifies it.
    let mut events = wallet.receive(None).await.unwrap().report.events;
    events.extend(wallet.receive(None).await.unwrap().report.events);
    let at = events
        .iter()
        .find_map(|e| match e {
            AppEvent::StoredEntryDropped { accepted_at_ms, .. } => Some(*accepted_at_ms),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{events:?}"));
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    let events = wallet.app_mut().unacknowledged_events().await.unwrap();
    assert!(!about_stored(&events), "{events:?}");
    (request, at)
}

#[tokio::test]
async fn test_stored_entry_dropped_unverified_asked_for_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, _) = dropped_unverified(&w, &mut wallet, &mut shop).await;
    w.homeserver.set_down(false);
    // The wallet asked bob to send it again; bob's shop answers, and the request arrives.
    wallet.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_stored_entry_dropped_unverified_mark_past_128_the_earliest_latest_goes() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let held = seed_marks(
        &mut wallet,
        "ud/",
        paykit_sdk::MAX_DROPPED_MARKS,
        w.clock.now_ms() - 1_000,
    )
    .await;
    dropped_unverified(&w, &mut wallet, &mut shop).await;
    let senders = mark_senders(&wallet, "ud/").await;
    assert_eq!(senders.len(), paykit_sdk::MAX_DROPPED_MARKS);
    assert!(senders.contains(&w.bob_id()));
    assert!(!senders.contains(&held[0]));
    assert!(senders.contains(&held[1]));
}

/// The marks of entries dropped unverified, whole.
async fn unverified_marks(app: &paykit_sdk::Paykit) -> Vec<serde_json::Value> {
    app.app()
        .protocol_scan(b"ud/")
        .await
        .unwrap()
        .into_iter()
        .map(|(_, value)| serde_json::from_slice::<serde_json::Value>(&value).unwrap()["d"].clone())
        .collect()
}

#[tokio::test]
async fn test_stored_entry_dropped_unverified_mark_asks_its_window_once_a_day() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (_, at) = dropped_unverified(&w, &mut wallet, &mut shop).await;
    w.homeserver.set_down(false);
    wallet.flush().await.unwrap();
    // Asked for what bob's inbox accepted from the mark's earliest time to its latest.
    assert_eq!(
        asked(&wallet, &w.bob_id()).await,
        vec![(Some(at), Some(at))]
    );
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    // A backup carries the mark whole, so a restore doesn't reset it.
    let held = unverified_marks(&wallet).await;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["earliest_ms"], at);
    assert_eq!(held[0]["latest_ms"], at);
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = paykit_sdk::Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(unverified_marks(&wallet).await, held);
    wallet.reconcile().await.unwrap();
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    // Asked again a day later while the mark lives.
    w.clock.advance_ms(DAY_MS);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
    assert_eq!(
        asked(&wallet, &w.bob_id()).await.last(),
        Some(&(Some(at), Some(at)))
    );
    // 30 days after its latest time, the mark goes.
    w.clock.set_ms(at + paykit_sdk::KEPT_MESSAGE_MS);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    assert!(unverified_marks(&wallet).await.is_empty());
}

/// Bob's request reaches alice's wallet, whose receive step's batch is applied and its
/// answer lost, kept past the window in which a resend's answer tells: the client stores it
/// as of unknown fate (Switch spec 12.5). `before` runs once the window has passed, before
/// the read that stores it.
async fn uncertain_request(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
    before: impl AsyncFnOnce(&mut paykit_sdk::Paykit),
) -> paykit_sdk::RequestRef {
    let mut unexpiring = draft("1", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let request = request_from(shop, w.alice_id(), unexpiring).await;
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    assert!(wallet.receive(None).await.is_err());
    w.clock.advance_ms(31 * pubky_switch_core::records::DAY_MS);
    before(wallet).await;
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        summary.report.events.iter().any(|e| matches!(
            e,
            AppEvent::Parked {
                reason: pubky_switch_client::app::ParkReason::Uncertain,
                ..
            }
        )),
        "{:?}",
        summary.report.events
    );
    request
}

#[tokio::test]
async fn test_stored_uncertain_effect_compensated_and_forgotten() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = uncertain_request(&w, &mut wallet, &mut shop, async |_| {}).await;
    // The record holds the request: the app's write found it in place, and the stored entry
    // went with its duty events.
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    let events = wallet.app_mut().unacknowledged_events().await.unwrap();
    assert!(!about_stored(&events), "{events:?}");
}

#[tokio::test]
async fn test_stored_uncertain_entry_released_after_compensation_declined() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // A message bob's switch never admits, bob having blocked alice, is given up first: a
    // duty event for the host, which the SDK doesn't acknowledge, so the stored entry's
    // events stay too.
    let mut manager = w
        .join(
            &w.bob,
            "manager",
            paykit_sdk::AppScopes::new()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    manager.app_mut().deny(&w.alice_id()).await.unwrap();
    let asked = wallet
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(1),
        )
        .await
        .unwrap();
    assert!(matches!(
        wallet.message_delivery(&asked).await.unwrap(),
        Some(paykit_sdk::MessageDelivery::WaitingForAdmission { .. })
    ));
    let request = uncertain_request(&w, &mut wallet, &mut shop, async |wallet| {
        // Given up however few tries confirmed it, past the longest wait (Switch spec 11).
        w.clock
            .advance_ms(pubky_switch_client::app::UNDELIVERABLE_MAX_MS);
        wallet.flush().await.unwrap();
    })
    .await;
    let events = wallet.app_mut().unacknowledged_events().await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AppEvent::MessageUndelivered { .. })),
        "{events:?}"
    );
    assert!(about_stored(&events), "{events:?}");
    let parked = wallet.app_mut().parked_entries().await.unwrap();
    assert_eq!(parked.len(), 1, "{parked:?}");
    // The host releases it anyway: its effect is in place, so the released run is declined.
    wallet
        .app_mut()
        .release_parked(parked[0].stored_id, &parked[0].hash)
        .await
        .unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        summary
            .report
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::MessageDropped { .. })),
        "{:?}",
        summary.report.events
    );
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

/// Whether alice's payer record of `request` holds a receipt.
async fn holds_receipt(wallet: &paykit_sdk::Paykit, request: &paykit_sdk::RequestRef) -> bool {
    let key = wallet.request(request).await.unwrap().record;
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    !value["receipt"].is_null()
}

#[tokio::test]
async fn test_stored_refused_receipt_written_beside_its_proof() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert!(!holds_receipt(&wallet, &request).await);
    wallet.receive(None).await.unwrap();
    assert!(holds_receipt(&wallet, &request).await);
}

#[tokio::test]
async fn test_stored_refused_receipt_from_before_a_joined_apps_first_listing_written_after() {
    // Alice's wallet pays bob's request; a phone joins, and its first read takes bob's
    // receipt, whose write is refused for good, before Paykit has listed the phone's
    // records. The stored entry is kept, and written after the listing.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let mut phone = Paykit::join(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        w.approve(&w.alice, "phone", &AppScopes::new().pay().build().unwrap()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    // No certificate is kept as listed under before the first listing.
    assert!(phone.app().protocol_get(b"lc").await.unwrap().is_none());
    // Bob's shop reads alice's directory again, which lists the phone, so the receipt goes
    // to it too.
    w.clock.advance_ms(11 * 60_000);
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    let summary = phone.receive(None).await.unwrap();
    assert!(about_stored(&summary.report.events), "{summary:?}");
    assert!(phone.app().protocol_get(b"lc").await.unwrap().is_some());
    assert!(!holds_receipt(&phone, &request).await);
    phone.receive(None).await.unwrap();
    assert!(holds_receipt(&phone, &request).await);
    assert!(phone.notices().await.unwrap().is_empty());
}

/// Alice's wallet pays `request`, the dispatch left in flight. Returns the attempt.
async fn pending(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    request: &paykit_sdk::RequestRef,
) -> paykit_lib::AttemptId {
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    attempt_id
}

/// Bob's receipt for the open `attempt_id`, naming a proof the record doesn't hold.
fn receipt_of_another_proof(
    w: &World,
    wallet: &paykit_sdk::Paykit,
    request: &paykit_sdk::RequestRef,
    attempt_id: paykit_lib::AttemptId,
) {
    let body = paykit_lib::bodies::PaymentReceipt {
        attempt_id,
        billing_period: None,
        metadata: None,
        payment_reference: None,
    }
    .to_body(2)
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_receipt",
        vec![[7; 32], request.request_hash],
        body,
    );
}

#[tokio::test]
async fn test_stored_refused_receipt_naming_another_proof_not_written() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(&w, &mut wallet, &request).await;
    // Its step, which settles the attempt, is refused, and so is its first later write.
    receipt_of_another_proof(&w, &wallet, &request, attempt_id);
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert_eq!(outcome_of(&mut wallet, attempt_id).await, Outcome::Open);
    // Written later, it isn't stored, and it settles the attempt it names, as at its first
    // read.
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "se/").await.is_empty());
    assert!(!holds_receipt(&wallet, &request).await);
    assert_eq!(outcome_of(&mut wallet, attempt_id).await, Outcome::Done);
}

/// The outcome of `attempt_id` on its claim, as the switch holds it.
async fn outcome_of(wallet: &mut paykit_sdk::Paykit, attempt_id: paykit_lib::AttemptId) -> Outcome {
    let obligation = wallet
        .payments()
        .await
        .unwrap()
        .into_iter()
        .find(|payment| payment.attempt_id == attempt_id)
        .unwrap()
        .obligation;
    let read = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    read.view.attempt(&attempt_id.to_bytes()).unwrap().outcome
}

#[tokio::test]
async fn test_stored_refused_unrequested_proof_credited_later() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(
            paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("0.001"))
                .with_options(
                    PayOptions::default()
                        .with_endpoint(crate::common::endpoint("btc-lightning-bolt12")),
                ),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    // The proof's step is refused, and the app's first write of it.
    refuse_writes(&w.bob, 2);
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap().is_empty());
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_stored_refused_note_given_up_past_its_window() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(&w, &mut wallet, &request).await;
    // A receipt naming a proof the record never holds: its step, which settles the
    // attempt, is refused, and so is its first later write.
    receipt_of_another_proof(&w, &wallet, &request, attempt_id);
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    // Past its window, the claim it settles can't be read, so it isn't written.
    w.alice.switch().fail_next(Fault::Error {
        path: "/claims/".into(),
        code: ErrorCode::PermissionMissing,
        current: serde_json::Value::Null,
    });
    wallet.receive(None).await.unwrap();
    // Given up: its note goes, its entry stays stored for the host, who is told.
    assert!(kept(&wallet, "se/").await.is_empty());
    let parked = wallet.app_mut().parked_entries().await.unwrap();
    assert_eq!(parked.len(), 1);
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, w.bob_id());
    assert_eq!(notices[0].request_hash, Some(request.request_hash));
    assert!(notices[0].msg_id.is_some());
    // Acknowledged, it goes at once: nothing raises it again.
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    assert!(kept(&wallet, "nt/").await.is_empty());
}

#[tokio::test]
async fn test_stored_refused_lists_hold_the_newer() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Each list's step is refused, and so is the write that would place the held list.
    for version in [w.clock.now_ms(), w.clock.now_ms() + 1] {
        w.list_to_alice(&mut shop, &[wallet.app_id()], version)
            .await;
        refuse_writes(&w.alice, 2);
        wallet.receive(None).await.unwrap();
        let held = wallet.held_lists().await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].version, version);
    }
}

/// Alice's wallet rejects bob's `request` while her switch takes no write, then refuses the
/// batch kept for it for good: a duty event about a kept batch, for the host.
async fn batch_refused(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    request: &paykit_sdk::RequestRef,
) {
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    let _ = wallet.reject_request(request, None).await;
    let before = refused_batches(wallet).await;
    refuse_writes(&w.alice, 1);
    let _ = wallet.receive(None).await;
    assert_eq!(refused_batches(wallet).await, before + 1);
}

/// How many duty events about a kept batch refused the wallet's host hasn't acknowledged.
async fn refused_batches(wallet: &mut paykit_sdk::Paykit) -> usize {
    wallet
        .app_mut()
        .unacknowledged_events()
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(e, AppEvent::BatchRefused { .. }))
        .count()
}

#[tokio::test]
async fn test_stored_dropped_entry_between_host_events_asked_for_once() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // A host event first, which the host doesn't acknowledge.
    batch_refused(&w, &mut wallet, &first).await;
    // Bob's next request is stored unverified, then dropped past 21 days.
    let mut unexpiring = draft("2", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let request = request_from(&mut shop, w.alice_id(), unexpiring).await;
    w.homeserver.set_down(true);
    // Past the 10 minutes bob's `identity.json` is cached for.
    w.clock.advance_ms(11 * 60_000);
    let _ = wallet.receive(None).await;
    w.clock
        .advance_ms(pubky_switch_client::app::DEFAULT_PARK_AFTER_MS + 1);
    let _ = wallet.receive(None).await;
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
    w.clock
        .advance_ms(pubky_switch_client::app::MAX_UNVERIFIED_AGE_MS);
    let (one, two) = (
        wallet.receive(None).await.unwrap(),
        wallet.receive(None).await.unwrap(),
    );
    assert!(
        one.report
            .events
            .iter()
            .chain(&two.report.events)
            .any(|e| matches!(e, AppEvent::StoredEntryDropped { .. })),
        "{:?} {:?}",
        one.report.events,
        two.report.events
    );
    // Asked once at first sight, though the host acknowledges nothing: a read again asks
    // nothing more.
    assert_eq!(one.resends + two.resends, 1);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    // A host event after it; the host acknowledges up to it once the receive returned.
    batch_refused(&w, &mut wallet, &first).await;
    let last = wallet
        .app_mut()
        .unacknowledged_events()
        .await
        .unwrap()
        .iter()
        .filter_map(AppEvent::id)
        .next_back()
        .unwrap();
    wallet.app_mut().ack_events(last).await.unwrap();
    // The re-send was asked once, at first sight: later reads within the day ask nothing
    // more.
    w.homeserver.set_down(false);
    wallet.flush().await.unwrap();
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_stored_event_taken_again_after_its_commit_failed() {
    let w = World::new();
    let fault = std::sync::Arc::new(pubky_switch_client::store::FaultStore::new(
        crate::common::store(),
        pubky_switch_client::store::Fault::None,
    ));
    let mut wallet = w
        .wallet_on(
            "wallet",
            std::sync::Arc::clone(&fault)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    // The commit that keeps the request's note, and the event's mark with it, fails, as a
    // crash before it would.
    refuse_writes(&w.alice, 1);
    fault.refuse_puts_as_too_large_once(b"p/se/");
    assert!(wallet.receive(None).await.is_err());
    assert!(kept(&wallet, "dt/").await.is_empty());
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_stored_refused_request_never_written_over_a_value_that_doesnt_read() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    // Meanwhile a value that doesn't read is written at the request's record.
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            paykit_lib::collections::Collection::PayerActivity.name(),
            paykit_lib::collections::Collection::PayerActivity.policy(None),
        ),
        &paykit_lib::derive::natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let entry = wallet
        .app_mut()
        .record_op(&target, Some(b"{\"v\":1}"), Vec::new())
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::new().entry(entry))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(held.content.value(), Some(&b"{\"v\":1}"[..]));
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Unrecorded
    );
}

#[tokio::test]
async fn test_stored_refused_request_never_written_over_a_value_awaiting_its_key() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    // The data epoch rotates, and the phone, holding the new epoch's key, writes the
    // request's record under it: the wallet doesn't hold that key yet.
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    // Its first read takes the new key, its next the request.
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            paykit_lib::collections::Collection::PayerActivity.name(),
            paykit_lib::collections::Collection::PayerActivity.policy(None),
        ),
        &paykit_lib::derive::natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let written = w.record_writes(&w.alice, &key);
    assert!(written >= 1);
    // The epoch rotates again, and the wallet gets the newest key alone: it could write the
    // record, but can't open what the phone wrote.
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    crate::unrequested::store_bundle_from(
        &w,
        wallet.app_id(),
        &wallet.app().cert().clone(),
        newest,
    );
    wallet.receive(None).await.unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        matches!(
            held.content,
            pubky_switch_client::app::RecordContent::Unreadable(
                pubky_switch_client::app::Unopened::KeyNotHeld { .. }
            )
        ),
        "{:?}",
        held.content
    );
    // Not written over while its key is awaited: the message stays kept.
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), written);
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    // Once its key comes the record reads, already holding the request: nothing is
    // written, and the kept message goes.
    crate::unrequested::store_bundle(&w, wallet.app_id(), &wallet.app().cert().clone());
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), written);
    assert!(kept(&wallet, "se/").await.is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_stored_refused_receipt_never_opens_a_deleted_record() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    // Meanwhile the payer's record of the request is deleted: a receipt never opens it.
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            paykit_lib::collections::Collection::PayerActivity.name(),
            paykit_lib::collections::Collection::PayerActivity.policy(None),
        ),
        &paykit_lib::derive::natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let entry = wallet
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::new().entry(entry))
        .await
        .unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let written = w.record_writes(&w.alice, &key);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), written);
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(
        held.content,
        pubky_switch_client::app::RecordContent::Deleted
    );
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
}

#[tokio::test]
async fn test_stored_refused_older_list_never_replaces_a_newer_held() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let newer = w.clock.now_ms() + 1;
    // The newer list's step is refused, then the older's, and the writes that would place
    // the held list: the newer stays held.
    for version in [newer, newer - 1] {
        w.list_to_alice(&mut shop, &[wallet.app_id()], version)
            .await;
        refuse_writes(&w.alice, 2);
        wallet.receive(None).await.unwrap();
        let held = wallet.held_lists().await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].version, newer);
    }
}

/// The wallet's kept receipt for `request`, waiting for a scope key: kept a day short of 30
/// days from its acceptance, then given up with the host's notice.
async fn given_up_after_30_days(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    request: &paykit_sdk::RequestRef,
) {
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - DAY_MS);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(wallet, "se/").await.len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    w.clock.advance_ms(2 * DAY_MS);
    wallet.receive(None).await.unwrap();
    assert!(kept(wallet, "se/").await.is_empty());
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, w.bob_id());
    assert_eq!(notices[0].request_hash, Some(request.request_hash));
}

#[tokio::test]
async fn test_stored_refused_receipt_waiting_for_its_records_key_given_up_after_30_days() {
    // A refused receipt is kept while the payer's record is held unopened, written again by
    // the phone under a key the wallet doesn't hold yet. It waits for the key a day short of
    // 30 days from its acceptance, and is then given up, the host told once: a key that comes
    // later finds nothing kept (Paykit spec 8.1). The phone took the receipt too, so the
    // record holds it whatever the wallet does.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    phone.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let value = crate::outcome_record::record_value(&mut phone, &request)
        .await
        .unwrap();
    crate::outcome_record::phone_writes(
        &mut phone,
        &request,
        Some(&serde_json::to_vec(&value).unwrap()),
    )
    .await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle_from(&w, wallet.app_id(), &cert, newest);
    wallet.receive(None).await.unwrap();
    given_up_after_30_days(&w, &mut wallet, &request).await;
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(kept(&wallet, "se/").await.is_empty());
    assert_eq!(wallet.notices().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_stored_refused_receipt_waiting_for_the_current_epochs_key_given_up_after_30_days() {
    // A refused receipt is kept, and then the data epoch rotates with no key for the wallet:
    // nothing can be sealed at the record yet. The receipt waits for the key a day short of
    // 30 days from its acceptance, and is then given up, the host told: a key that comes
    // later doesn't bring it back (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    wallet.receive(None).await.unwrap();
    given_up_after_30_days(&w, &mut wallet, &request).await;
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(kept(&wallet, "se/").await.is_empty());
    assert!(!holds_receipt(&wallet, &request).await);
}
