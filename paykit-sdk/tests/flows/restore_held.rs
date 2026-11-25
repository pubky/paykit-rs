//! Original deliveries carried across a backup and restore (Paykit spec 7.2, 9).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use paykit_lib::bodies::{PaymentProof, PrivatePaymentList, PrivatePaymentListRequest};
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{ActivityRecord, ActivitySide, AllowanceRecord, ListPublicationRecord};
use paykit_lib::AttemptId;
use paykit_sdk::{
    AllowanceStatus, AppScopes, HeldMessages, NoticeKind, PayOptions, PayOutcome, PayeeStatus,
    Paykit, PaykitBackup, PaykitConfig, Rail, RailOutcome, ReceiptsOf, Refusal, RequestRef,
    RequestStatus,
};
use pubky_dispatch_client::app::{AppError, AppEvent, OutgoingMessage, SENT_RETENTION_MS};
use pubky_dispatch_client::clock::Clock;
use pubky_dispatch_client::error::StoreError;
use pubky_dispatch_client::store::{Fault as StoreFault, FaultStore, StateStore};
use pubky_dispatch_client::testing::{Fault, Router, ScriptedDispatch, TestSigner};
use pubky_dispatch_client::transport::{BoxFuture, Request, Response, Transport};

use crate::common::{draft, endpoint, receive_message, request_from, store, World};
use crate::{allowances, outcome_record, periods, publishing};

pub(super) async fn restore(
    w: &World,
    who: &TestSigner,
    storage: Arc<dyn StateStore>,
    backup: &PaykitBackup,
) -> Paykit {
    Paykit::restore(
        w.env(who, storage),
        PaykitConfig::default(),
        backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap()
}

/// A receive's held messages when it decides nothing and none is held.
fn nothing_held() -> HeldMessages {
    HeldMessages {
        still_held: Some(0),
        ..HeldMessages::default()
    }
}

async fn unknown(app: &mut Paykit, id: &str) {
    assert!(app
        .app_mut()
        .pending_duty_events()
        .await
        .unwrap()
        .iter()
        .any(|d| matches!(&d.event, AppEvent::DeliveryUnknown { msg_id, .. } if msg_id == id)));
}

async fn held(app: &Paykit, id: &str) -> bool {
    app.app()
        .held_messages("paykit")
        .await
        .unwrap()
        .iter()
        .any(|m| m == id)
}

async fn withdrawn(app: &mut Paykit) {
    let counts = app.receive(None).await.unwrap().held_messages;
    assert_eq!(counts.withdrawn, 1, "{counts:?}");
}

async fn decisions_through_listing(app: &mut Paykit) -> HeldMessages {
    let mut counts = nothing_held();
    for _ in 0..8 {
        let read = app.receive(None).await.unwrap();
        counts.released += read.held_messages.released;
        counts.withdrawn += read.held_messages.withdrawn;
        counts.still_held = read.held_messages.still_held;
        if !read.report.more && read.records_unlisted.is_none() {
            assert!(!read.report.stopped && !app.app().listing_owed().await.unwrap());
            return counts;
        }
    }
    panic!("restore listing did not finish")
}

async fn released(app: &mut Paykit, id: &str) {
    assert!(held(app, id).await);
    for _ in 0..8 {
        let report = app.receive(None).await.unwrap();
        if report.held_messages.released == 1 {
            assert!(!held(app, id).await);
            assert!(app.message_delivery(id).await.unwrap().is_some());
            app.flush().await.unwrap();
            return;
        }
        assert!(report.held_messages.still_held.unwrap() > 0);
    }
    panic!("the held message {id} was never released");
}

async fn cancellation_backup(w: &World) -> (Paykit, RequestRef, String, PaykitBackup) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    let id = shop.cancel_sent_request(&sale, None).await.unwrap();
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    (wallet, request, id, backup)
}

async fn cancellation(w: &World) -> (Paykit, RequestRef, String, Paykit) {
    let (wallet, request, id, backup) = Box::pin(cancellation_backup(w)).await;
    let shop = Box::pin(restore(w, &w.bob, store(), &backup)).await;
    (wallet, request, id, shop)
}

#[tokio::test]
async fn test_restore_held_payee_cancellation_delivered_once() {
    let w = World::new();
    let (mut wallet, request, stop, mut shop) = Box::pin(cancellation(&w)).await;
    Box::pin(released(&mut shop, &stop)).await;
    receive_message(&mut wallet, &stop).await;
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    assert_eq!(
        shop.receive(None).await.unwrap().held_messages,
        nothing_held()
    );
    shop.flush().await.unwrap();
    assert!(!wallet.receive(None).await.unwrap().applied.contains(&stop));
    let sent = shop
        .app()
        .sent_to(&w.alice_id(), 0, u64::MAX)
        .await
        .unwrap();
    assert!(sent
        .iter()
        .any(|s| s.msg_id == stop && s.resent_at_ms.is_empty()));
}

#[tokio::test]
async fn test_restore_held_waits_for_the_rest_of_the_feed() {
    use pubky_dispatch_client::app::{AppConfig, FEED_PAGES_PER_READ};

    let w = World::new();
    let (mut wallet, request, stop, backup) = Box::pin(cancellation_backup(&w)).await;
    let config = PaykitConfig::default().with_app(AppConfig {
        page_size: 1,
        ..AppConfig::default()
    });
    let mut shop = Box::pin(Paykit::restore(
        w.env(&w.bob, store()),
        config,
        &backup,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    assert!(held(&shop, &stop).await);

    // Each unrelated inbox entry fills one page. The rejection follows a full read.
    for _ in 0..FEED_PAGES_PER_READ {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop.app_id()],
            "chat.item",
            Vec::new(),
            Default::default(),
        );
    }
    let before_rejection = w.bob.dispatch().seq();
    let rejection = wallet.reject_request(&request, None).await.unwrap();
    wallet.flush().await.unwrap();

    // A carried stop waits for the whole feed because the counterparty's stop for
    // the same request makes ours unnecessary (Paykit spec 7.2).
    let mut partial_feed = false;
    let mut read = shop.receive(None).await.unwrap();
    for _ in 0..8 {
        if !read.report.more {
            break;
        }
        assert!(!read.report.stopped);
        partial_feed |= read.report.held_cursor.is_some()
            && read.report.cursor.seq <= before_rejection
            && read.records_unlisted.is_none()
            && !read.scope_keys_awaited
            && !shop.app().listing_owed().await.unwrap();
        let counts = read.held_messages;
        assert_eq!(
            (
                counts.released,
                counts.withdrawn,
                counts.still_held.unwrap()
            ),
            (0, 0, 1)
        );
        assert!(held(&shop, &stop).await);
        read = shop.receive(None).await.unwrap();
    }
    assert!(
        partial_feed,
        "expected a listed shop with the rejection still unread"
    );
    assert!(!read.report.more && !read.report.stopped);
    let counts = read.held_messages;
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 1, 0)
    );
    assert!(!held(&shop, &stop).await);
    let sale = shop
        .app_mut()
        .read_record(&crate::restore::payee_record(&request))
        .await
        .unwrap()
        .unwrap();
    let record = ActivityRecord::read(
        sale.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payee,
    )
    .unwrap();
    assert_eq!(record.response.unwrap().message().msg_id, rejection);
    shop.receive(None).await.unwrap();
    assert!(stop_notice_ids(&shop, &stop).await.is_empty());
}

#[tokio::test]
async fn test_restore_held_unreadable_stop_waits_then_releases() {
    let w = World::new();
    let (mut wallet, request, stop, mut shop) = Box::pin(cancellation(&w)).await;
    let key = shop
        .app()
        .record_key(&crate::restore::payee_record(&request))
        .unwrap();
    w.bob.dispatch().fail_next(Fault::Unreachable {
        path: format!("records/{}", pubky_dispatch_core::enc::b64(&key)),
        after: 0,
    });
    let counts = shop.receive(None).await.unwrap().held_messages;
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 0, 1)
    );
    assert!(!wallet.receive(None).await.unwrap().applied.contains(&stop));
    Box::pin(released(&mut shop, &stop)).await;
    receive_message(&mut wallet, &stop).await;
}

// stored.rs:471 takes the outstanding withdrawal duty after the wallet is reloaded.
#[tokio::test]
async fn test_restore_held_open_request_keeps_stop_notice_across_reload() {
    let w = World::new();
    let (wallet, request, id, backup) = Box::pin(unrecorded_rejection(&w)).await;
    drop(wallet);
    let storage = store();
    let mut wallet = Box::pin(restore(&w, &w.alice, storage.clone(), &backup)).await;
    assert!(held(&wallet, &id).await);
    withdrawn(&mut wallet).await;
    assert!(!held(&wallet, &id).await);
    let app_id = wallet.app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, storage, app_id, w.clock.clone())
        .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert_eq!(stop_notice_ids(&wallet, &id).await.len(), 1);
}

#[tokio::test]
async fn test_restore_held_past_deadline_is_given_up_by_dispatch() {
    let w = World::new();
    let (mut wallet, _, stop, mut shop) = Box::pin(cancellation(&w)).await;
    assert!(held(&shop, &stop).await);
    w.clock.advance_ms(SENT_RETENTION_MS + 1);
    assert_eq!(
        shop.receive(None).await.unwrap().held_messages,
        nothing_held()
    );
    unknown(&mut shop, &stop).await;
    assert!(!wallet.receive(None).await.unwrap().applied.contains(&stop));
}

// restore_held.rs:247 withdraws a stop past its signed window while a scope key is missing.
#[tokio::test]
async fn test_restore_held_past_thirty_days_withdrawn_while_the_gate_is_closed() {
    use crate::common::HOUR_MS;

    let w = World::new();
    let (mut wallet, request, stop, _shop, _) = Box::pin(carried_rejection(&w, store())).await;
    let signed_at = w.clock.now_ms();
    w.clock.advance_ms(2 * HOUR_MS);
    wallet
        .app_mut()
        .read_record_fresh(&outcome_record::payer_record(&request))
        .await
        .unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);

    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    read_restored_client(&mut wallet).await;
    let epoch = w
        .alice
        .next_doc(|body| body.data_epoch += 1)
        .unwrap()
        .data_epoch();
    wallet.app_mut().refresh_identity().await.unwrap();
    w.clock.set_ms(signed_at + SENT_RETENTION_MS + HOUR_MS);
    wallet.app_mut().flush_message(&stop).await.unwrap();
    assert!(held(&wallet, &stop).await);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().data_epoch(), epoch);
    assert!(wallet.app().scope_key(epoch, "paykit_pay").is_none());
    assert!(!held(&wallet, &stop).await);
    unknown(&mut wallet, &stop).await;
    assert!(stop_notice_ids(&wallet, &stop).await.is_empty());
}

// Even a stop owed only by the restored store expires and is reported after 30 days
// while its record cannot be read (Paykit spec 7.2).
#[tokio::test]
async fn test_restore_held_store_only_stop_without_a_readable_record_ends_after_thirty_days() {
    let w = World::new();
    let storage = store();
    let mut wallet = w.wallet_on("wallet", storage.clone()).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let key = wallet
        .app()
        .record_key(&outcome_record::payer_record(&request))
        .unwrap();
    let backup = wallet.backup().await.unwrap();
    wallet.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    let stop = queue_unrecorded_rejection(&mut wallet, &request).await;
    drop(wallet);

    let mut wallet = Box::pin(restore(&w, &w.alice, storage, &backup)).await;
    let fail_read = || {
        w.alice.dispatch().fail_next(Fault::Unreachable {
            path: format!("records/{}", pubky_dispatch_core::enc::b64(&key)),
            after: 0,
        })
    };
    fail_read();
    let before = wallet.receive(None).await.unwrap().held_messages;
    assert_eq!(
        (
            before.released,
            before.withdrawn,
            before.still_held.unwrap()
        ),
        (0, 0, 1)
    );
    assert!(held(&wallet, &stop).await);

    w.clock
        .advance_ms(SENT_RETENTION_MS + crate::common::HOUR_MS);
    fail_read();
    let after = wallet.receive(None).await.unwrap().held_messages;
    assert_eq!((after.released, after.still_held.unwrap()), (0, 0));
    assert!(!held(&wallet, &stop).await);
    assert!(wallet.message_delivery(&stop).await.unwrap().is_none());
    let duties = wallet.app_mut().pending_duty_events().await.unwrap();
    assert!(duties.iter().any(|d| matches!(
        &d.event,
        AppEvent::DeliveryUnknown { msg_id, .. } | AppEvent::MessageUndelivered { msg_id, .. }
            if msg_id == &stop
    )));
}

#[tokio::test]
async fn test_restore_held_payer_rejection_waits_while_payee_blocked() {
    let w = World::new();
    let mut wallet = w
        .join(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    let stop = wallet.reject_request(&request, None).await.unwrap();
    wallet.app_mut().release_queued(&w.bob_id()).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    Box::pin(released(&mut wallet, &stop)).await;
    assert!(!shop.receive(None).await.unwrap().applied.contains(&stop));
    assert!(wallet
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .iter()
        .any(|q| q.msg_id == stop && q.waits() && !q.held));
}

#[tokio::test]
async fn test_restore_held_permanent_allowance_end_reaches_counterparty() {
    let w = World::new();
    let mut contacts = w
        .join(
            &w.bob,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
        )
        .await;
    let (mut shop, mut wallet, _exec, _phone, theirs, mine) =
        Box::pin(allowances::executed(&w)).await;
    shop.receive(None).await.unwrap();
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    let stop = shop.end_allowance(&theirs).await.unwrap();
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    // Without the user origin this exact recorded end follows the generated-end gate.
    shop.app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_dispatch_client::app::LocalWrite::Delete {
                key: format!("end/{stop}").into_bytes(),
            }],
        )
        .await
        .unwrap();
    let backup = shop.backup().await.unwrap();
    contacts.app_mut().deny(&w.alice_id()).await.unwrap();
    drop(shop);
    let mut shop = Box::pin(restore(&w, &w.bob, store(), &backup)).await;
    Box::pin(released(&mut shop, &stop)).await;
    assert!(!wallet.receive(None).await.unwrap().applied.contains(&stop));
    assert!(shop
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .iter()
        .any(|q| q.msg_id == stop && !q.held));
    contacts.app_mut().undeny(&w.alice_id()).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    shop.receive(None).await.unwrap();
    shop.flush_message_in_delivery(&stop).await.unwrap();
    receive_message(&mut wallet, &stop).await;
    assert_eq!(
        wallet.allowance(&mine).await.unwrap().status,
        AllowanceStatus::Ended
    );
}

#[tokio::test]
async fn test_restore_held_request_withdrawn_and_reported_unknown() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = Box::pin(restore(&w, &w.bob, store(), &backup)).await;
    assert!(held(&shop, &sent.msg_id).await);
    withdrawn(&mut shop).await;
    unknown(&mut shop, &sent.msg_id).await;
    assert!(!held(&shop, &sent.msg_id).await);
    assert!(!wallet
        .receive(None)
        .await
        .unwrap()
        .applied
        .contains(&sent.msg_id));
}

// restore_held.rs:372 keeps a fresh withdrawal for the carried list's recipient.
#[tokio::test]
async fn test_restore_held_nonempty_list_replaced_by_fresh_withdrawal() {
    for in_place in [false, true] {
        Box::pin(carried_list(in_place)).await;
    }
}

async fn carried_list(in_place: bool) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let storage = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            Default::default(),
            storage.clone(),
        )
        .await;
    let to = w.alice_id();
    shop.app_mut().invite(&to).await.unwrap();
    shop.app_mut().hold_queued(&to).await.unwrap();
    shop.set_list_endpoints(publishing::offered("lno1first"))
        .await
        .unwrap();
    shop.publish_list_to(to).await.unwrap();
    let key = shop
        .app()
        .record_key(&publishing::target(&shop, &to, 1))
        .unwrap();
    let held_record = shop.app().shared_record(&key).await.unwrap().unwrap();
    let original = ListPublicationRecord::read(held_record.content.value().unwrap()).unwrap();
    shop.app_mut().release_queued(&to).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let target = if in_place { storage } else { store() };
    let mut shop = Box::pin(restore(&w, &w.bob, target, &backup)).await;
    assert_eq!(held(&shop, &original.msg_id).await, !in_place);
    withdrawn(&mut shop).await;
    // The host is told of the carried list's delivery by the client's give-up event, in the
    // next receive's report, whatever became of the list's notice (Paykit spec 7.2, 9).
    let next = shop.receive(None).await.unwrap();
    assert!(
        next.report.events.iter().any(|e| matches!(e,
            AppEvent::DeliveryUnknown { msg_id, .. } | AppEvent::MessageUndelivered { msg_id, .. }
                if msg_id == &original.msg_id)),
        "{:?}",
        next.report.events
    );
    shop.flush().await.unwrap();
    let (fresh, list) = sent_list(&shop, &to).await;
    assert_ne!(fresh, original.msg_id);
    assert!(list.payment_endpoints.is_empty());
    assert!(list.version.counter() > original.counter);
    receive_message(&mut wallet, &fresh).await;
}

async fn sent_list(shop: &Paykit, to: &[u8; 32]) -> (String, PrivatePaymentList) {
    let sent = shop.app().sent_to(to, 0, u64::MAX).await.unwrap();
    let lists: Vec<_> = sent
        .iter()
        .filter(|s| s.kind == "paykit.private_payment_list")
        .collect();
    let [empty] = lists.as_slice() else {
        panic!("expected one fresh withdrawal")
    };
    let msg = pubky_dispatch_core::message::Message::parse(
        empty.plaintext.json(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    let paykit_lib::Body::PrivatePaymentList(list) = paykit_lib::Body::parse(&msg).unwrap() else {
        unreachable!()
    };
    (empty.msg_id.clone(), list)
}

fn paid_proof(outcome: PayOutcome) -> (AttemptId, String) {
    let PayOutcome::Paid {
        attempt_id,
        proof_msg_id: Some(proof),
        ..
    } = outcome
    else {
        panic!("expected a finished payment: {outcome:?}")
    };
    (attempt_id, proof)
}

/// Alice pays bob's request; the backup carries the finished payment's queued proof.
async fn carried_proof(
    w: &World,
) -> (
    Paykit,
    Paykit,
    RequestRef,
    (AttemptId, String),
    PaykitBackup,
) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    wallet.app_mut().release_queued(&w.bob_id()).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    (wallet, shop, request, paid_proof(outcome), backup)
}

// restore_held.rs:542 releases the exact proof retained for the paid attempt.
#[tokio::test]
async fn test_restore_held_proof_of_a_finished_payment_is_released_and_credited_once() {
    let w = World::new();
    let (wallet, mut shop, _request, (attempt, proof), backup) = Box::pin(carried_proof(&w)).await;
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    // The payer's record holds the exact proof and no receipt: it goes (Paykit spec 7.2).
    Box::pin(released(&mut wallet, &proof)).await;
    receive_message(&mut shop, &proof).await;
    let sales = shop.payee_activity().await.unwrap();
    let [sale] = sales.as_slice() else {
        panic!("expected one sale")
    };
    shop.confirm_payment(&sale.record).await.unwrap().unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Receipted
    );
    assert_eq!(w.rail.pay_calls(&attempt), 1);
}

async fn deliver_proof_and_receipt(wallet: &mut Paykit, shop: &mut Paykit, proof: &str) -> String {
    wallet.flush().await.unwrap();
    receive_message(shop, proof).await;
    let sale = shop.payee_activity().await.unwrap()[0].record;
    let receipt = shop.confirm_payment(&sale).await.unwrap().unwrap();
    receive_message(wallet, &receipt).await;
    receipt
}

// restore_held.rs:535 checks for the attempt's receipt before releasing the proof.
#[tokio::test]
async fn test_restore_held_proof_withdrawn_once_its_receipt_is_recorded() {
    let w = World::new();
    let (mut wallet, mut shop, _request, (attempt, proof), backup) =
        Box::pin(carried_proof(&w)).await;
    // After the backup the proof goes, the payee credits it and the receipt is recorded.
    Box::pin(deliver_proof_and_receipt(&mut wallet, &mut shop, &proof)).await;
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    let counts = decisions_through_listing(&mut wallet).await;
    // A receipt for its attempt leaves nothing owed (Paykit spec 7.2); the payment's
    // acceptance, carried beside it, is withdrawn as every message but a stop or proof.
    assert_eq!((counts.released, counts.withdrawn), (0, 2), "{counts:?}");
    assert!(!held(&wallet, &proof).await);
    assert_eq!(w.rail.pay_calls(&attempt), 1);
}

async fn assert_receipt_unplaced(wallet: &Paykit, key: &str) {
    let note = wallet
        .app()
        .protocol_get("paykit", key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let note: serde_json::Value = serde_json::from_slice(&note).unwrap();
    assert_eq!(note["d"]["placed"], false);
}

// restore_held.rs:713 keeps the proof held while its payee's stored receipt awaits placement.
#[tokio::test]
async fn test_restore_held_proof_waits_for_a_stored_receipt_not_placed() {
    let w = World::new();
    let (mut wallet, mut shop, request, (_, proof), backup) = Box::pin(carried_proof(&w)).await;
    let key = wallet.request(&request).await.unwrap().record;
    wallet.flush().await.unwrap();
    receive_message(&mut shop, &proof).await;
    let sale = shop.payee_activity().await.unwrap()[0].record;
    let receipt = shop.confirm_payment(&sale).await.unwrap().unwrap();
    let note = format!(
        "se/{}/{receipt}",
        pubky_dispatch_core::enc::b64(&w.bob_id())
    );
    w.alice.dispatch().keep_passed_entries();
    crate::common::refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_receipt_unplaced(&wallet, &note).await;
    drop(wallet);

    let (mut wallet, placement) =
        Box::pin(restore_before_placement(&w, &w.alice, &backup, key)).await;
    let counts = decisions_through_listing(&mut wallet).await;
    assert_eq!(
        (counts.released, counts.withdrawn, counts.still_held),
        (0, 1, Some(1))
    );
    assert_receipt_unplaced(&wallet, &note).await;
    assert!(held(&wallet, &proof).await);
    placement.refusing.store(false, Ordering::Relaxed);
    withdrawn(&mut wallet).await;
    assert!(!held(&wallet, &proof).await);
}

// restore_held.rs:546 withdraws a proof missing from the payer's record.
#[tokio::test]
async fn test_restore_held_paid_proof_withdrawn_then_credited_once() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("expected an open attempt")
    };
    w.rail.settle(&attempt_id, true);
    let Some(RailOutcome::Paid(paid)) = w.rail.payment(attempt_id).await.unwrap() else {
        panic!("expected payment")
    };
    // The backup carries a signed proof but the paid attempt still lacks its outcome write.
    let body = PaymentProof {
        attempt_id,
        billing_period: None,
        payment_endpoint_identifier: endpoint("btc-lightning-bolt11"),
        payment_app_id: shop.app_id(),
        list_version: None,
        amount: None,
        allowance_id: None,
        label: None,
        payment_reference: None,
        proof: paid.proof,
    }
    .to_body(1)
    .unwrap();
    let mut proof = OutgoingMessage::new(w.bob_id(), "paykit.payment_proof", body);
    proof.refs.push(request.request_hash);
    let id = wallet.app_mut().queue(proof).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    assert!(held(&wallet, &id).await);
    withdrawn(&mut wallet).await;
    unknown(&mut wallet, &id).await;
    let resumed = wallet.reconcile().await.unwrap();
    let [PayOutcome::Paid {
        proof_msg_id: Some(replacement),
        ..
    }] = resumed.resolved.as_slice()
    else {
        panic!("expected a recorded proof: {resumed:?}")
    };
    assert_ne!(replacement, &id);
    receive_message(&mut shop, replacement).await;
    let sales = shop.payee_activity().await.unwrap();
    let [sale] = sales.as_slice() else {
        panic!("expected one sale")
    };
    assert_eq!(sale.attempt_id, Some(attempt_id));
    let receipt = shop.confirm_payment(&sale.record).await.unwrap().unwrap();
    receive_message(&mut wallet, &receipt).await;
    wallet.resume_payments().await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Receipted
    );
    assert_eq!(
        wallet
            .receipts(ReceiptsOf::Request(request))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

// pay.rs:3835 leaves the payment owed until the restored acceptance has been decided.
#[tokio::test]
async fn test_restore_held_acceptance_keeps_its_payment_owed_until_decided() {
    use paykit_sdk::{Resolution, Stage};
    use pubky_dispatch_client::app::UserConfirmed;

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let bob = w.bob_id();
    let kind = "paykit.payment_request_acceptance";
    wallet.app_mut().hold_queued(&bob).await.unwrap();
    let attempt_id = Box::pin(crate::stored::pending(&w, &mut wallet, &request)).await;
    let queued = wallet.app().queued_messages("paykit").await.unwrap();
    let original = queued.into_iter().find(|q| q.kind == kind).unwrap().msg_id;
    w.rail.settle(&attempt_id, true);
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    wallet.app_mut().release_queued(&bob).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);

    // Once the attempt can be resolved by another app, the phone records it as paid.
    w.clock.advance_ms(2 * crate::common::HOUR_MS);
    let mut phone = w.wallet("phone").await;
    phone
        .resolve_attempt(
            &obligation,
            attempt_id,
            Resolution::Done,
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    read_restored_client(&mut wallet).await;
    wallet.resume_payments().await.unwrap();
    assert!(held(&wallet, &original).await);
    assert_eq!(wallet.payments().await.unwrap()[0].stage, Stage::Paying);
    withdrawn(&mut wallet).await;
    assert!(!held(&wallet, &original).await);
    unknown(&mut wallet, &original).await;

    wallet.resume_payments().await.unwrap();
    assert_eq!(wallet.payments().await.unwrap()[0].stage, Stage::Finished);
    wallet.flush().await.unwrap();
    let sent = wallet.app().sent_to(&bob, 0, u64::MAX).await.unwrap();
    let replacement = sent.iter().find(|m| m.kind == kind).unwrap();
    assert_ne!(replacement.msg_id, original);
    receive_message(&mut shop, &replacement.msg_id).await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_restore_held_acceptance_resend_withdrawn_keeps_the_sent_original() {
    let w = World::new();
    let storage = store();
    let mut wallet = w.wallet_on("wallet", storage.clone()).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    assert!(matches!(
        wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap(),
        PayOutcome::Pending { .. }
    ));
    let sent = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap();
    let acceptance = |m: &&pubky_dispatch_client::app::SentMessage| {
        m.kind == "paykit.payment_request_acceptance"
    };
    let original = &sent.iter().find(acceptance).unwrap().msg_id;
    receive_message(&mut shop, original).await;
    let backup = wallet.backup().await.unwrap();
    wallet.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    wallet.app_mut().resend(original).await.unwrap();
    wallet.app_mut().release_queued(&w.bob_id()).await.unwrap();
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, storage, &backup)).await;
    assert!(held(&wallet, original).await);
    withdrawn(&mut wallet).await;
    // Restored in place, the store keeps its sent copy: the payee holds the original, so
    // the resume signs no replacement (Dispatch spec 12.4, Paykit spec 7.2).
    assert!(matches!(
        wallet.app().delivery(original).await.unwrap(),
        Some(pubky_dispatch_client::app::Delivery::Accepted { .. })
    ));
    wallet.reconcile().await.unwrap();
    wallet.resume_payments().await.unwrap();
    let sent = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap();
    let acceptances: Vec<_> = sent.iter().filter(acceptance).map(|m| &m.msg_id).collect();
    assert_eq!(acceptances, [original]);
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// Alice's wallet, restored from a backup whose outbox carries her recorded rejection of
/// Bob's request, and Bob's shop. Alice's store is `base` before the restore.
async fn carried_rejection(
    w: &World,
    base: Arc<dyn StateStore>,
) -> (Paykit, RequestRef, String, Paykit, PaykitBackup) {
    let mut wallet = w.wallet_on("wallet", base).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    let id = wallet.reject_request(&request, None).await.unwrap();
    wallet.app_mut().release_queued(&w.bob_id()).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    (wallet, request, id, shop, backup)
}

async fn queue_unrecorded_rejection(wallet: &mut Paykit, request: &RequestRef) -> String {
    let mut rejection = OutgoingMessage::new(
        request.payee,
        "paykit.payment_request_rejection",
        Default::default(),
    );
    rejection.refs.push(request.request_hash);
    wallet.app_mut().queue(rejection).await.unwrap()
}

async fn unrecorded_rejection(w: &World) -> (Paykit, RequestRef, String, PaykitBackup) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let stop = queue_unrecorded_rejection(&mut wallet, &request).await;
    let backup = wallet.backup().await.unwrap();
    (wallet, request, stop, backup)
}

// restore_held.rs:480 withdraws silently for the counterparty's stop.
#[tokio::test]
async fn test_restore_held_cancellation_withdrawn_silently_after_the_payer_rejects() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    let id = shop.cancel_sent_request(&sale, None).await.unwrap();
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    // The payer rejects the request before the cancellation reaches it.
    wallet.reject_request(&request, None).await.unwrap();
    wallet.flush().await.unwrap();
    let mut shop = Box::pin(restore(&w, &w.bob, store(), &backup)).await;
    assert!(held(&shop, &id).await);
    let mut counts = nothing_held();
    for _ in 0..8 {
        let report = shop.receive(None).await.unwrap();
        counts.withdrawn += report.held_messages.withdrawn;
        counts.released += report.held_messages.released;
        if !report.report.more {
            break;
        }
    }
    // The payer's own rejection makes the cancellation moot: nothing is owed and the user
    // isn't asked to stop again (Paykit spec 7.2).
    assert_eq!((counts.released, counts.withdrawn), (0, 1), "{counts:?}");
    assert!(!held(&shop, &id).await);
    shop.receive(None).await.unwrap();
    assert!(!shop
        .notices()
        .await
        .unwrap()
        .iter()
        .any(|n| n.kind == NoticeKind::StopNotRecorded));
}

#[tokio::test]
async fn test_restore_held_rejection_waits_while_its_record_is_paused() {
    let w = World::new();
    let base = store();
    let (mut wallet, request, id, _shop, _) =
        Box::pin(carried_rejection(&w, Arc::clone(&base))).await;
    let record = wallet.request(&request).await.unwrap().record;
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    // Confirmed before the backup, the record is paused again by the restore (Paykit spec 6).
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    assert!(held(&wallet, &id).await);
    let counts = wallet.receive(None).await.unwrap().held_messages;
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 0, 1),
        "{counts:?}"
    );
    assert!(held(&wallet, &id).await);
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    Box::pin(released(&mut wallet, &id)).await;
}

/// The reply grants for `peer` written at `who`'s dispatch.
fn reply_grants(who: &TestSigner, peer: [u8; 32]) -> usize {
    use pubky_dispatch_core::admission::PeerReason;
    use pubky_dispatch_core::ops::OpKind;
    who.dispatch()
        .applied()
        .iter()
        .filter(|op| {
            matches!(&op.body().kind, OpKind::AllowPut { pubky, reason: PeerReason::Reply } if *pubky == peer)
        })
        .count()
}

#[tokio::test]
async fn test_restore_held_released_stop_adds_no_reply_grant() {
    let w = World::new();
    let (wallet, _request, id, _shop, backup) = Box::pin(carried_rejection(&w, store())).await;
    drop(wallet);
    // The user removes bob's reply grant after the backup.
    let mut contacts = w
        .join(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
        )
        .await;
    contacts.app_mut().remove_reply(&w.bob_id()).await.unwrap();
    let before = reply_grants(&w.alice, w.bob_id());
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    Box::pin(released(&mut wallet, &id)).await;
    assert!(matches!(
        wallet.message_delivery(&id).await.unwrap(),
        Some(paykit_sdk::MessageDelivery::Accepted { .. })
    ));
    // The stop only narrows: it neither writes nor renews a reply grant (Paykit spec 7.2).
    assert_eq!(reply_grants(&w.alice, w.bob_id()), before);
}

#[tokio::test]
async fn test_restore_held_allowance_end_waits_while_its_record_is_paused() {
    let w = World::new();
    let (mut shop, _wallet, _exec, _phone, theirs, _mine) =
        Box::pin(allowances::executed(&w)).await;
    shop.receive(None).await.unwrap();
    // Seen divergent: the record is paused until its user confirms it (Paykit spec 6).
    w.bob
        .dispatch()
        .make_divergent([0x5a; 16], theirs, [1; 32])
        .unwrap();
    shop.receive(None).await.unwrap();
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    // The end is a stop, so it is written over the paused record; its write clears the view.
    let stop = shop.end_allowance(&theirs).await.unwrap();
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = Box::pin(restore(&w, &w.bob, store(), &backup)).await;
    // Paykit's first read completes, its records listed: the end still waits, held for
    // the paused record alone.
    let mut listed = false;
    for _ in 0..8 {
        let report = shop.receive(None).await.unwrap();
        assert_eq!(report.held_messages.released, 0);
        if !report.report.more && report.records_unlisted.is_none() && !report.scope_keys_awaited {
            listed = true;
            break;
        }
    }
    assert!(listed);
    assert!(held(&shop, &stop).await);
    crate::divergent::confirm(&mut shop, &theirs).await.unwrap();
    Box::pin(released(&mut shop, &stop)).await;
}

#[tokio::test]
async fn test_restore_held_stop_waits_for_the_listing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    // A shop with more collections than one read lists.
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            AppScopes::new().request().allowance().build().unwrap(),
        )
        .await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    let id = shop.cancel_sent_request(&sale, None).await.unwrap();
    // A second request, withdrawn once decided, which no record read holds back.
    let second = shop
        .send_request(w.alice_id(), draft("2", w.clock.now_ms()))
        .await
        .unwrap()
        .msg_id;
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = Box::pin(restore(&w, &w.bob, store(), &backup)).await;
    // Nothing is decided while Paykit's records aren't listed (Paykit spec 7.2).
    let mut unlisted = 0;
    let (mut released_now, mut withdrawn_now) = (0, 0);
    for _ in 0..8 {
        let report = shop.receive(None).await.unwrap();
        if report.records_unlisted.is_some() {
            unlisted += 1;
            assert_eq!(
                (
                    report.held_messages.released,
                    report.held_messages.withdrawn
                ),
                (0, 0),
                "{report:?}"
            );
        }
        released_now += report.held_messages.released;
        withdrawn_now += report.held_messages.withdrawn;
        if released_now == 1 && withdrawn_now == 1 {
            break;
        }
    }
    // The shop's listing took more than one receive.
    assert!(unlisted >= 2, "{unlisted}");
    assert_eq!((released_now, withdrawn_now), (1, 1));
    assert!(!held(&shop, &id).await && !held(&shop, &second).await);
    shop.flush().await.unwrap();
    receive_message(&mut wallet, &id).await;
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
}

// restore_held.rs:499 keeps an accepted subscription open for a stop. stored.rs:471 reports it.
#[tokio::test]
async fn test_restore_held_rejection_of_a_subscription_accepted_elsewhere_is_told() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(
        &mut shop,
        w.alice_id(),
        crate::periods::monthly(w.clock.now_ms(), None),
    )
    .await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    // The wallet's rejection is signed and queued; its record write never happened.
    let id = queue_unrecorded_rejection(&mut wallet, &request).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    phone.accept_subscription(&request).await.unwrap();
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    assert_eq!(decisions_through_listing(&mut wallet).await.withdrawn, 1);
    wallet.receive(None).await.unwrap();
    // The subscription stays open: the user is told the record no longer shows the
    // rejection, and may cancel it (Paykit spec 7.2).
    assert_eq!(stop_notice_ids(&wallet, &id).await.len(), 1);
}

#[tokio::test]
async fn test_restore_held_payer_only_cancellation_without_its_record_is_withdrawn() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    // A cancellation with no side kept and no payer record: a payer-only app can't hold it
    // as a payee's, so it is withdrawn rather than left to wait (Paykit spec 7.2).
    let mut cancellation = OutgoingMessage::new(
        w.bob_id(),
        "paykit.payment_request_cancellation",
        Default::default(),
    );
    cancellation.refs.push([7; 32]);
    let id = wallet.app_mut().queue(cancellation).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    let mut withdrawn = 0;
    for _ in 0..8 {
        let report = wallet.receive(None).await.unwrap();
        withdrawn += report.held_messages.withdrawn;
        if !report.report.more && report.records_unlisted.is_none() {
            break;
        }
    }
    assert_eq!(withdrawn, 1);
    assert!(!held(&wallet, &id).await);
}

async fn pay_with_period_details(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
) -> (RequestRef, PeriodRef, AttemptId, String) {
    let draft = periods::monthly(w.clock.now_ms(), None).with_per_period_endpoints();
    let period = PeriodRef::from(draft.recurrence.as_ref().unwrap().periods(2).unwrap()[1]);
    let request = request_from(shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let acceptance = wallet.accept_subscription(&request).await.unwrap();
    receive_message(shop, &acceptance.msg_id).await;
    let sale = shop.payee_activity().await.unwrap()[0].record;
    w.clock.set_ms(period.starts_at);
    let mut details = periods::january_details(period.starts_at + crate::common::HOUR_MS);
    details.billing_period = period;
    let details = shop.send_period_details(&sale, details).await.unwrap();
    receive_message(wallet, &details.msg_id).await;
    wallet.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    let payment = Box::pin(wallet.pay_period(
        &request,
        &period,
        PayOptions::default().with_period_details(&details.msg_id),
    ))
    .await;
    let (attempt, proof) = paid_proof(payment.outcome.unwrap());
    let key = crate::period_payments::period_key(wallet, &request, period);
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let record = ActivityRecord::read(
        held.content.value().unwrap(),
        &w.alice_id(),
        ActivitySide::Payer,
    )
    .unwrap();
    let selected = record.period_details.unwrap();
    assert_eq!(selected.message().msg_id, details.msg_id);
    let signed = record.proof.unwrap();
    assert_eq!(signed.message().msg_id, proof);
    assert_eq!(
        signed.message().refs,
        [request.request_hash, selected.hash()]
    );
    wallet.app_mut().release_queued(&w.bob_id()).await.unwrap();
    (request, period, attempt, proof)
}

// restore_held.rs:518 locates a proof with [request, details] in the payer's period record.
#[tokio::test]
async fn test_restore_held_two_reference_period_proof_is_released_and_credited_once() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, period, attempt, proof) =
        Box::pin(pay_with_period_details(&w, &mut wallet, &mut shop)).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);

    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    Box::pin(released(&mut wallet, &proof)).await;
    receive_message(&mut shop, &proof).await;
    let key = shop.payee_activity().await.unwrap()[0].periods[0].record;
    let writes = w.record_writes(&w.bob, &key);
    let receipt = shop.confirm_payment(&key).await.unwrap().unwrap();
    assert_eq!(
        shop.confirm_payment(&key).await.unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(w.record_writes(&w.bob, &key), writes + 1);
    receive_message(&mut wallet, &receipt).await;
    assert_eq!(
        wallet
            .receipts(ReceiptsOf::Period(request, period))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(w.rail.pay_calls(&attempt), 1);
}

// restore_held.rs:358 releases the original ID on either restore. requests.rs:1540
// returns the receipt already recorded when the payee confirms the same proof again.
#[tokio::test]
async fn test_restore_held_same_proof_from_two_restores_credits_once() {
    let w = World::new();
    let (wallet, mut shop, request, (attempt, proof), backup) = Box::pin(carried_proof(&w)).await;
    drop(wallet);
    let mut first = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    Box::pin(released(&mut first, &proof)).await;
    receive_message(&mut shop, &proof).await;
    drop(first);
    // Keep the receipt from the payer until both restores have sent the proof.
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    let writes = w.record_writes(&w.bob, &sale);
    let receipt = shop.confirm_payment(&sale).await.unwrap().unwrap();
    let mut second = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    Box::pin(released(&mut second, &proof)).await;
    assert!(matches!(
        second.message_delivery(&proof).await.unwrap(),
        Some(paykit_sdk::MessageDelivery::Accepted { .. })
    ));
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.confirm_payment(&sale).await.unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(w.record_writes(&w.bob, &sale), writes + 1);
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    shop.flush().await.unwrap();
    let sent = shop
        .app()
        .sent_to(&w.alice_id(), 0, u64::MAX)
        .await
        .unwrap();
    let receipt_ids: Vec<_> = sent
        .iter()
        .filter(|m| m.kind == "paykit.payment_receipt")
        .map(|m| &m.msg_id)
        .collect();
    assert_eq!(receipt_ids, [&receipt]);
    receive_message(&mut second, &receipt).await;
    let receipts = second.receipts(ReceiptsOf::Request(request)).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].msg_id, receipt);
    assert_eq!(w.rail.pay_calls(&attempt), 1);
}

async fn queued_shop_end(w: &World) -> (Paykit, Paykit, [u8; 32], String) {
    let (mut shop, wallet, _exec, _phone, record, _) = Box::pin(allowances::executed(w)).await;
    shop.receive(None).await.unwrap();
    shop.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    let end = shop.end_allowance(&record).await.unwrap();
    (shop, wallet, record, end)
}

async fn allowance_record(app: &Paykit, key: &[u8; 32]) -> AllowanceRecord {
    let held = app.app().shared_record(key).await.unwrap().unwrap();
    AllowanceRecord::read(held.content.value().unwrap(), &app.identity()).unwrap()
}

// A carried Allowance end waits for the key to its record. Once the record opens,
// its exact signed end can be released (Paykit spec 7.2).
#[tokio::test]
async fn test_restore_held_allowance_end_waits_for_the_key_its_record_needs() {
    use pubky_dispatch_client::app::{RecordContent, Unopened};
    use pubky_dispatch_core::ops::{KeyBundlePut, OpKind};

    let w = World::new();
    let (mut shop, _wallet, key, end) = Box::pin(queued_shop_end(&w)).await;
    let id = shop.allowance(&key).await.unwrap().allowance_id;
    let target = allowances::allowances_target(&w.alice_id(), &id);
    let record = allowance_record(&shop, &key).await;
    assert_eq!(record.end.as_ref().unwrap().message().msg_id, end);
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    let cert = shop.app().cert().clone();
    drop(shop);

    let sealed_at = w.bob.next_doc(|b| b.data_epoch += 1).unwrap().data_epoch();
    let mut writer = w
        .join(
            &w.bob,
            "second-shop",
            AppScopes::new().allowance().build().unwrap(),
        )
        .await;
    Box::pin(allowances::write_record(
        &mut writer,
        &target,
        Some(&record.to_json().unwrap()),
    ))
    .await;

    // The shop can finish its listing at the current epoch while the preceding epoch is missing.
    let current = w.bob.next_doc(|b| b.data_epoch += 1).unwrap().data_epoch();
    for part in pubky_dispatch_core::records::seal_bundle_parts(
        w.bob.identity().key(),
        &cert,
        current,
        current,
        &|s| w.bob.spec().has_keys(s),
    )
    .unwrap()
    {
        w.bob
            .dispatch()
            .signer_op(OpKind::KeyBundlePut(KeyBundlePut {
                target_app_id: cert.app_id(),
                bundle: part.bundle,
                first_epoch: part.first_epoch,
                last_epoch: part.last_epoch,
            }))
            .unwrap();
    }

    let mut shop = Box::pin(restore(&w, &w.bob, store(), &backup)).await;
    assert!(held(&shop, &end).await);
    shop.app_mut().refresh_identity().await.unwrap();
    shop.app_mut().fetch_key_bundle().await.unwrap();
    assert!(shop.app().scope_key(current, "paykit_allowance").is_some());
    assert!(shop
        .app()
        .scope_key(sealed_at, "paykit_allowance")
        .is_none());
    let counts = decisions_through_listing(&mut shop).await;
    let sealed = shop.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(
        sealed.content,
        RecordContent::Unreadable(Unopened::KeyNotHeld {
            data_epoch: sealed_at
        })
    );
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 0, 1)
    );
    assert!(held(&shop, &end).await);

    crate::certificate::store_bundle_for(&w.bob, cert.app_id(), &cert);
    shop.app_mut().fetch_key_bundle().await.unwrap();
    Box::pin(released(&mut shop, &end)).await;
}

// allowances/delivery.rs:294 withdraws a conflicted Allowance's end even when it is exact.
#[tokio::test]
async fn test_restore_held_conflicted_allowance_withdraws_its_recorded_end_silently() {
    let w = World::new();
    let (mut shop, mut wallet, record, end) = Box::pin(queued_shop_end(&w)).await;
    let prior = allowance_record(&shop, &record).await;
    let mut proposal = prior.proposal.message().body.clone();
    proposal.insert("proposer_role".into(), "allower".into());
    let second = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.allowance_proposal",
        Vec::new(),
        proposal,
    );
    receive_message(&mut shop, &second).await;
    let current = allowance_record(&shop, &record).await;
    assert_eq!(current.conflicted.unwrap().message().msg_id, second);
    assert_eq!(current.end.unwrap().message().msg_id, end);
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);

    let mut shop = Box::pin(restore(&w, &w.bob, store(), &backup)).await;
    assert!(held(&shop, &end).await);
    let before = w.alice.dispatch().entries_from(&w.bob_id());
    let counts = decisions_through_listing(&mut shop).await;
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 1, 0)
    );
    shop.flush().await.unwrap();
    let read = wallet.receive(None).await.unwrap();
    assert!(!allowances::received(&read, "paykit.allowance_end"));
    assert_eq!(w.alice.dispatch().entries_from(&w.bob_id()), before);
    assert!(shop.message_delivery(&end).await.unwrap().is_none());
    shop.receive(None).await.unwrap();
    assert!(!shop
        .notices()
        .await
        .unwrap()
        .iter()
        .any(|n| n.kind == NoticeKind::StopNotRecorded));
}

async fn confirm_held_divergent(app: &mut Paykit, record: [u8; 32], stop: &str) {
    let counts = decisions_through_listing(app).await;
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 0, 1)
    );
    let views = app.divergent_records().await.unwrap();
    assert!(views
        .iter()
        .any(|v| v.record == record && v.paused && v.divergent));
    assert!(held(app, stop).await);
    crate::divergent::confirm(app, &record).await.unwrap();
    let counts = app.receive(None).await.unwrap().held_messages;
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (1, 0, 0)
    );
    assert!(!held(app, stop).await);
    assert!(
        app.app()
            .shared_record(&record)
            .await
            .unwrap()
            .unwrap()
            .divergent
    );
}

// restore_held.rs:595 checks the payee record's pause. Line 587 permits a confirmed view.
#[tokio::test]
async fn test_restore_held_divergent_cancellation_released_after_confirmation() {
    let w = World::new();
    let (_, request, stop, mut shop) = Box::pin(cancellation(&w)).await;
    let record = shop
        .app()
        .record_key(&crate::restore::payee_record(&request))
        .unwrap();
    w.bob
        .dispatch()
        .make_divergent([0x5a; 16], record, [1; 32])
        .unwrap();
    Box::pin(confirm_held_divergent(&mut shop, record, &stop)).await;
}

// allowances/delivery.rs:277 waits on the pause. Line 281 accepts the confirmed divergent view.
#[tokio::test]
async fn test_restore_held_divergent_allowance_end_released_after_confirmation() {
    let w = World::new();
    let (mut shop, _wallet, record, end) = Box::pin(queued_shop_end(&w)).await;
    shop.app_mut().release_queued(&w.alice_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    w.bob
        .dispatch()
        .make_divergent([0x5a; 16], record, [1; 32])
        .unwrap();
    let mut shop = Box::pin(restore(&w, &w.bob, store(), &backup)).await;
    Box::pin(confirm_held_divergent(&mut shop, record, &end)).await;
}

async fn mark_payer_loss(wallet: &mut Paykit, request: &RequestRef) -> ActivityRecord {
    let target = outcome_record::payer_record(request);
    let key = wallet.app().record_key(&target).unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let own = wallet.identity();
    let mut record =
        ActivityRecord::read(held.content.value().unwrap(), &own, ActivitySide::Payer).unwrap();
    record.lost_to_delete = Some([73; 32]);
    let written = wallet
        .app_mut()
        .put_record(&target, &record.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    let current =
        ActivityRecord::read(written.content.value().unwrap(), &own, ActivitySide::Payer).unwrap();
    assert_eq!(current.lost_to_delete, record.lost_to_delete);
    current
}

// restore_held.rs:615 returns the marked record so line 542 can release its exact proof.
#[tokio::test]
async fn test_restore_held_lost_to_delete_releases_the_recorded_proof() {
    let w = World::new();
    let (mut wallet, mut shop, request, (attempt, proof), backup) =
        Box::pin(carried_proof(&w)).await;
    let record = mark_payer_loss(&mut wallet, &request).await;
    assert_eq!(record.proof.unwrap().message().msg_id, proof);
    assert!(record.receipt.is_none());
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    Box::pin(released(&mut wallet, &proof)).await;
    receive_message(&mut shop, &proof).await;
    assert_eq!(w.rail.pay_calls(&attempt), 1);
}

// restore_held.rs:482 releases the recorded rejection even when the record names a delete.
#[tokio::test]
async fn test_restore_held_lost_to_delete_releases_the_recorded_rejection() {
    let w = World::new();
    let (mut wallet, request, stop, mut shop, backup) =
        Box::pin(carried_rejection(&w, store())).await;
    let record = mark_payer_loss(&mut wallet, &request).await;
    assert_eq!(record.response.unwrap().message().msg_id, stop);
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    Box::pin(released(&mut wallet, &stop)).await;
    receive_message(&mut shop, &stop).await;
}

// restore_held.rs:535 withdraws a proof when the marked record holds its attempt's receipt.
#[tokio::test]
async fn test_restore_held_lost_to_delete_withdraws_the_receipted_proof() {
    let w = World::new();
    let (mut wallet, mut shop, request, (attempt, proof), backup) =
        Box::pin(carried_proof(&w)).await;
    let receipt = Box::pin(deliver_proof_and_receipt(&mut wallet, &mut shop, &proof)).await;
    let record = mark_payer_loss(&mut wallet, &request).await;
    assert_eq!(record.proof.unwrap().message().msg_id, proof);
    assert_eq!(record.receipt.unwrap().message().msg_id, receipt);
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    assert!(held(&wallet, &proof).await);
    let counts = decisions_through_listing(&mut wallet).await;
    // The acceptance and the receipted proof are both in the backup.
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 2, 0)
    );
    assert!(!held(&wallet, &proof).await);
    assert_eq!(w.rail.pay_calls(&attempt), 1);
}

async fn stop_notice_ids(app: &Paykit, id: &str) -> Vec<String> {
    app.notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.kind == NoticeKind::StopNotRecorded && n.msg_id.as_deref() == Some(id))
        .map(|n| n.id)
        .collect()
}

// restore_held.rs:499 checks whether the marked request is open when its rejection is absent.
#[tokio::test]
async fn test_restore_held_lost_to_delete_withdraws_the_unrecorded_rejection() {
    let w = World::new();
    let (mut wallet, request, stop, backup) = Box::pin(unrecorded_rejection(&w)).await;
    let record = mark_payer_loss(&mut wallet, &request).await;
    assert!(record.response.is_none() && record.cancellation.is_none());
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    assert!(held(&wallet, &stop).await);
    withdrawn(&mut wallet).await;
    assert!(!held(&wallet, &stop).await);
    wallet.receive(None).await.unwrap();
    assert_eq!(stop_notice_ids(&wallet, &stop).await.len(), 1);
}

async fn queue_list_request(wallet: &mut Paykit, to: [u8; 32]) -> String {
    let body = PrivatePaymentListRequest {
        app_id: None,
        held_version: None,
    }
    .to_body()
    .unwrap();
    wallet
        .app_mut()
        .queue(OutgoingMessage::new(
            to,
            "paykit.private_payment_list_request",
            body,
        ))
        .await
        .unwrap()
}

/// Fail the Dispatch commit that records this specific delivery as given up.
async fn fail_withdrawal(app: &mut Paykit, storage: &FaultStore, id: &str) {
    storage.fail_put_once(format!("outgone/{id}").as_bytes());
    for _ in 0..8 {
        match app.receive(None).await {
            Err(paykit_sdk::PaykitError::Client(error))
                if matches!(error.unkept(), AppError::Store(StoreError::Unavailable)) =>
            {
                assert!(held(app, id).await);
                return;
            }
            Ok(read) if read.report.more || read.records_unlisted.is_some() => {
                assert_eq!(read.held_messages.released, 0);
                assert_eq!(read.held_messages.withdrawn, 0);
            }
            result => panic!("expected a failure withdrawing {id}: {result:?}"),
        }
    }
    panic!("the client never tried to withdraw {id}");
}

/// Signed writes from the generation about to be lost, enough for one recovery upload
/// to make the client list its collections (Dispatch wire 7.4.14).
async fn recovery_upload_for_listing(
    w: &World,
    wallet: &mut Paykit,
) -> pubky_dispatch_client::client::answers::RecoveryUpload {
    use pubky_dispatch_client::app::{Collection, RecordTarget, MAX_RECORD_MARKS};
    use pubky_dispatch_client::client::answers::{RecoveryUpload, UploadedOp};
    use pubky_dispatch_core::ops::sign_batch;

    let keys = w.alice.identity().app_keys("wallet");
    // These records trigger the listing without changing the request the stop refers to.
    let collection = Collection::new(
        "notes",
        paykit_lib::Collection::PayerActivity.fixed_policy(),
    );
    let mut upload = RecoveryUpload::default();
    for i in 0..=MAX_RECORD_MARKS {
        let target = RecordTarget::new(collection.clone(), &i.to_be_bytes());
        let op = wallet
            .app_mut()
            .record_op(&target, Some(b"{}"), Vec::new())
            .await
            .unwrap();
        let signed = sign_batch(
            &keys,
            &w.alice_id(),
            Some(wallet.app_id()),
            wallet.app().own_identity().generation(),
            (i as u128).to_be_bytes(),
            vec![op],
        )
        .unwrap()
        .pop()
        .unwrap();
        upload.records.push(UploadedOp {
            op: signed.signed().clone(),
            cert: wallet.app().cert().signed().clone(),
            proof: None,
        });
    }
    upload
}

// A carried stop waits for the client's listing even when the certificate and its
// scope keys are unchanged (Paykit spec 7.2, Dispatch spec 12.4).
#[tokio::test]
async fn test_restore_held_waits_for_a_listing_the_client_owes() {
    use pubky_dispatch_client::client::{Credentials, DispatchClient};
    use pubky_dispatch_client::testing::DISPATCH_ORIGIN;

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let upload = recovery_upload_for_listing(&w, &mut wallet).await;
    let service = crate::provider_loss::declare_loss(&w);
    Box::pin(crate::provider_loss::follow_loss(&mut wallet)).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let stop = queue_unrecorded_rejection(&mut wallet, &request).await;
    let cert = wallet.app().cert().clone();
    let epoch = wallet.app().data_epoch();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);

    let storage = Arc::new(FaultStore::new(store(), StoreFault::None));
    let mut wallet = Box::pin(restore(&w, &w.alice, storage.clone(), &backup)).await;
    // Complete the restore listing, keeping the rejection held at its withdrawal.
    fail_withdrawal(&mut wallet, &storage, &stop).await;
    assert!(!wallet.app().listing_owed().await.unwrap());
    let before = service.tip_seq();
    let client = DispatchClient::new(
        DISPATCH_ORIGIN,
        w.alice_id(),
        Credentials::App {
            identity: w.alice_id(),
            keys: w.alice.identity().app_keys("wallet"),
            cert: cert.clone(),
        },
        Some(service.service_key()),
        w.router.clone(),
        w.clock.clone(),
    )
    .unwrap();
    let answer = client.recovery_upload(&upload, false).await.unwrap();
    assert_eq!(answer.result.unwrap().records.len(), upload.records.len());

    // An earlier listing page cannot cover the upload. The direct read takes its
    // transition, then the Paykit receive gets the same earlier listing snapshot.
    let earlier_page = || {
        service.fail_next(Fault::Replace {
            path: "records?collection".into(),
            member: "seq".into(),
            value: before.into(),
        })
    };
    earlier_page();
    wallet.app_mut().receive(None).await.unwrap();
    assert!(wallet.app().listing_owed().await.unwrap());
    assert!(held(&wallet, &stop).await);
    earlier_page();
    let read = wallet.receive(None).await.unwrap();
    assert!(wallet.app().listing_owed().await.unwrap());
    assert!(read.records_unlisted.is_none() && !read.report.stopped);
    assert_eq!(wallet.app().cert().hash(), cert.hash());
    assert_eq!(wallet.app().data_epoch(), epoch);
    assert!(wallet.app().scope_key(epoch, "paykit_pay").is_some());
    let counts = read.held_messages;
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 0, 1)
    );
    assert!(held(&wallet, &stop).await);

    let counts = decisions_through_listing(&mut wallet).await;
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 1, 0)
    );
    assert!(!held(&wallet, &stop).await);
}

// restore_held.rs:206 requires the current scope keys even under the already-listed certificate.
#[tokio::test]
async fn test_restore_held_waits_for_the_scope_key_after_epoch_rotation() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let ask = queue_list_request(&mut wallet, w.bob_id()).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let storage = Arc::new(FaultStore::new(store(), StoreFault::None));
    let mut wallet = Box::pin(restore(&w, &w.alice, storage.clone(), &backup)).await;
    // Leave a message with no record-dependent decision in an otherwise finished listing.
    fail_withdrawal(&mut wallet, &storage, &ask).await;
    let cert = wallet.app().cert().clone();
    let next = w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    wallet.app_mut().refresh_identity().await.unwrap();
    let read = wallet.receive(None).await.unwrap();
    assert!(read.report.held_cursor.is_some() && !read.report.stopped);
    assert!(read.records_unlisted.is_none() && !wallet.app().listing_owed().await.unwrap());
    assert_eq!(wallet.app().cert().hash(), cert.hash());
    assert_eq!(wallet.app().data_epoch(), next.data_epoch());
    assert!(wallet
        .app()
        .scope_key(next.data_epoch(), "paykit_pay")
        .is_none());
    let counts = read.held_messages;
    assert_eq!(
        (
            counts.released,
            counts.withdrawn,
            counts.still_held.unwrap()
        ),
        (0, 0, 1)
    );
    assert!(held(&wallet, &ask).await);
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    wallet.app_mut().fetch_key_bundle().await.unwrap();
    withdrawn(&mut wallet).await;
    assert!(!held(&wallet, &ask).await);
}

async fn expire_before_first_read(w: &World, app: &mut Paykit, id: &str) {
    assert!(held(app, id).await);
    w.homeserver.set_down(true);
    w.clock.advance_ms(SENT_RETENTION_MS + 1);
    let error = app.receive(None).await.unwrap_err();
    assert!(matches!(
        error.refusal(),
        Some(Refusal::IdentityUnavailable(_))
    ));
    assert!(app.app().restore_held_cursor().is_none());
    assert!(!held(app, id).await);
    assert!(app.message_delivery(id).await.unwrap().is_none());
    unknown(app, id).await;
}

// paykit.rs:1084 expires held deliveries before line 1105 can refuse the first read.
#[tokio::test]
async fn test_restore_held_expiry_is_told_while_own_identity_is_unavailable() {
    let w = World::new();
    let (_, _, stop, mut shop) = Box::pin(cancellation(&w)).await;
    expire_before_first_read(&w, &mut shop, &stop).await;
}

async fn queue_unpublished_list(w: &World, shop: &mut Paykit, to: [u8; 32]) -> String {
    let body = PrivatePaymentList {
        version: crate::common::list_version(1, 1),
        expires_at: w.clock.now_ms() + SENT_RETENTION_MS,
        payment_endpoints: [(
            endpoint("btc-lightning-bolt12"),
            paykit_lib::bodies::ListEndpoint {
                payload: paykit_lib::PaymentEndpointPayload::new("lno1old").unwrap(),
                single_use: false,
            },
        )]
        .into(),
    }
    .to_body()
    .unwrap();
    shop.app_mut()
        .queue(OutgoingMessage::new(
            to,
            "paykit.private_payment_list",
            body,
        ))
        .await
        .unwrap()
}

// restore_held.rs:227 remembers the list recipient before line 232 expires its only delivery.
#[tokio::test]
async fn test_restore_held_expired_list_without_history_still_sends_a_fresh_withdrawal() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let to = w.alice_id();
    shop.app_mut().invite(&to).await.unwrap();
    let original = queue_unpublished_list(&w, &mut shop, to).await;
    assert!(shop
        .app()
        .records_of("private_list_publications")
        .await
        .unwrap()
        .is_empty());
    assert!(shop
        .app()
        .protocol_scan("paykit", b"lhi/")
        .await
        .unwrap()
        .is_empty());
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = Box::pin(restore(&w, &w.bob, store(), &backup)).await;
    assert!(shop
        .app()
        .protocol_scan("paykit", b"lrec/")
        .await
        .unwrap()
        .is_empty());
    expire_before_first_read(&w, &mut shop, &original).await;
    w.homeserver.set_down(false);
    crate::common::finish_listing(&mut shop).await;
    shop.flush().await.unwrap();
    let (fresh, list) = sent_list(&shop, &to).await;
    assert_ne!(fresh, original);
    assert!(list.payment_endpoints.is_empty());
    receive_message(&mut wallet, &fresh).await;
}

// notices.rs:959 hides the pending notice. Taking the withdrawal duty exposes it
// through stored.rs:471 and notices.rs:993.
#[tokio::test]
async fn test_restore_held_stop_notice_shown_once_after_withdrawal_is_told() {
    let w = World::new();
    let (wallet, _, stop, backup) = Box::pin(unrecorded_rejection(&w)).await;
    drop(wallet);
    let storage = Arc::new(FaultStore::new(store(), StoreFault::None));
    let mut wallet = Box::pin(restore(&w, &w.alice, storage.clone(), &backup)).await;
    fail_withdrawal(&mut wallet, &storage, &stop).await;
    assert!(stop_notice_ids(&wallet, &stop).await.is_empty());
    // Once the client no longer holds the stop, the pass that withdrew it shows the
    // notice, once (`show_pending_stops`).
    withdrawn(&mut wallet).await;
    assert!(!held(&wallet, &stop).await);
    unknown(&mut wallet, &stop).await;
    let ids = stop_notice_ids(&wallet, &stop).await;
    let [id] = ids.as_slice() else {
        panic!("expected one stop notice: {ids:?}")
    };
    wallet.acknowledge_notice(id).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(stop_notice_ids(&wallet, &stop).await.is_empty());
}

// restore_held.rs `mark_after`: whichever listing of the client's held messages fails,
// the owed mark goes only after a sweep, so the stop's notice is still shown, once.
#[tokio::test]
async fn test_restore_held_stop_notice_shown_whichever_hold_listing_fails() {
    // The deciding receive lists them twice: at the pass's start (0) and for its
    // end-of-pass sweep (1).
    for after in 0..2 {
        let w = World::new();
        let (wallet, _, stop, backup) = Box::pin(unrecorded_rejection(&w)).await;
        drop(wallet);
        let storage = Arc::new(FaultStore::new(store(), StoreFault::None));
        let mut wallet = Box::pin(restore(&w, &w.alice, storage.clone(), &backup)).await;
        storage.fail_scan(b"outheld/", after);
        // The receive that meets the failed listing fails with it; the ones after go on.
        let mut failed = 0;
        for _ in 0..4 {
            failed += usize::from(wallet.receive(None).await.is_err());
        }
        assert_eq!(failed, 1, "after {after}");
        assert!(!held(&wallet, &stop).await, "after {after}");
        assert_eq!(
            stop_notice_ids(&wallet, &stop).await.len(),
            1,
            "after {after}"
        );
    }
}

/// A restored wallet whose carried stop stays held: a scope key it needs isn't held yet, so
/// nothing is decided, on a store that can fail a listing.
async fn held_while_a_key_is_missing(w: &World) -> (Paykit, Arc<FaultStore>, String) {
    let (wallet, _, stop, _shop, backup) = Box::pin(carried_rejection(w, store())).await;
    drop(wallet);
    let storage = Arc::new(FaultStore::new(store(), StoreFault::None));
    let mut wallet = Box::pin(restore(w, &w.alice, storage.clone(), &backup)).await;
    read_restored_client(&mut wallet).await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    wallet.app_mut().refresh_identity().await.unwrap();
    assert_eq!(
        wallet.receive(None).await.unwrap().held_messages.still_held,
        Some(1)
    );
    (wallet, storage, stop)
}

// paykit.rs read_before_effects: a store failure the expiry step meets before the read is
// left to the read, which goes on; it doesn't meet that failure, so the receive succeeds.
#[tokio::test]
async fn test_restore_held_expiry_failure_before_the_read_leaves_it_to_the_read() {
    let w = World::new();
    let (mut wallet, storage, stop) = Box::pin(held_while_a_key_is_missing(&w)).await;
    // The receive's first listing of the outbox is the expiry step's, before the read.
    storage.fail_scan(b"out/", 0);
    let read = wallet.receive(None).await.unwrap();
    assert!(!read.report.stopped);
    assert_eq!(read.held_messages.still_held, Some(1));
    // The failure was met: the next listing passes.
    assert!(wallet.app().queued_messages("paykit").await.is_ok());
    assert!(held(&wallet, &stop).await);
}

// paykit.rs held_messages_now: a summary that can't list the held messages reports them
// as unknown, the receive still returning its read; the next one lists them again.
#[tokio::test]
async fn test_restore_held_still_held_unknown_when_the_summary_cannot_list() {
    let mut unknown = 0;
    // Every listing of the outbox the receive makes, failed in turn.
    for after in 0..24 {
        let w = World::new();
        let (mut wallet, storage, _) = Box::pin(held_while_a_key_is_missing(&w)).await;
        storage.fail_scan(b"out/", after);
        if let Ok(read) = wallet.receive(None).await {
            if read.held_messages.still_held.is_none() {
                unknown += 1;
                let next = wallet.receive(None).await.unwrap();
                assert_eq!(next.held_messages.still_held, Some(1), "after {after}");
            }
        }
    }
    // The summary's own listing, the receive's last, reports unknown; earlier ones are the
    // expiry step's, left to the read, or fail the receive.
    assert!(unknown > 0);
}

// Held messages are decided only in a Paykit receive, after its own read succeeded
// (`Paykit::held_decisions_open`): a read made through the client, as another protocol's,
// decides nothing, and nor does a flush until a Paykit receive has read.
#[tokio::test]
async fn test_restore_held_direct_client_read_does_not_open_paykit_decisions() {
    let w = World::new();
    let (wallet, _, stop, mut shop, backup) = Box::pin(carried_rejection(&w, store())).await;
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    wallet.app_mut().refresh_identity().await.unwrap();
    for _ in 0..8 {
        let direct = wallet.app_mut().receive(None).await.unwrap();
        if direct.held_cursor.is_some() {
            break;
        }
        assert!(direct.more && !direct.stopped);
    }
    assert!(wallet.app().restore_held_cursor().is_some());
    // Calls that make no read of Paykit's own decide nothing.
    wallet.flush().await.unwrap();
    wallet.resume_payments().await.unwrap();
    assert!(held(&wallet, &stop).await);
    w.alice.dispatch().fail_next(Fault::Unreachable {
        path: "/feed".into(),
        after: 0,
    });
    assert!(wallet.receive(None).await.is_err());
    assert!(held(&wallet, &stop).await);
    assert!(!shop.receive(None).await.unwrap().applied.contains(&stop));
    // Paykit's own complete read decides it: the whole feed read, nothing left to list.
    let read = wallet.receive(None).await.unwrap();
    assert!(read.report.held_cursor.is_some() && !read.report.more);
    assert!(read.records_unlisted.is_none() && !wallet.app().listing_owed().await.unwrap());
    assert_eq!(read.held_messages.released, 1);
    wallet.flush().await.unwrap();
    receive_message(&mut shop, &stop).await;
}

// A stop of the wallet isn't swallowed by the expiry step (`Paykit::read_before_effects`):
// the call ends with the stop, typed, and a stopped wallet gives nothing up.
#[tokio::test]
async fn test_restore_held_expiry_leaves_a_wallet_stop_typed() {
    let w = World::new();
    let (wallet, _, stop, _shop, backup) = Box::pin(carried_rejection(&w, store())).await;
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    assert!(held(&wallet, &stop).await);
    w.clock
        .advance_ms(SENT_RETENTION_MS + crate::common::HOUR_MS);
    wallet
        .app_mut()
        .raise_rollback("paykit", "a rollback this test raises")
        .await
        .unwrap();
    let error = wallet.receive(None).await.unwrap_err();
    assert!(
        matches!(
            &error,
            paykit_sdk::PaykitError::Client(
                pubky_dispatch_client::app::AppError::Alarm(_)
                    | pubky_dispatch_client::app::AppError::Refused(
                        pubky_dispatch_client::app::Refusal::Stopped
                    )
            )
        ),
        "{error:?}"
    );
    assert!(held(&wallet, &stop).await);
}

/// Let the Dispatch finish its restore read while Paykit still owes its first held-message pass.
async fn read_restored_client(app: &mut Paykit) {
    app.app_mut().refresh_identity().await.unwrap();
    for _ in 0..8 {
        let read = app.app_mut().receive(None).await.unwrap();
        assert!(!read.stopped);
        if !read.more && !app.app().listing_owed().await.unwrap() {
            assert!(app.app().restore_held_cursor().is_some());
            return;
        }
    }
    panic!("the Dispatch did not finish the restore read");
}

/// Refuse just the placement write. The restore can still read the same record afresh.
struct RefusePlacement {
    router: Arc<Router>,
    service: Arc<ScriptedDispatch>,
    record: [u8; 32],
    refusing: AtomicBool,
}

impl Transport for RefusePlacement {
    fn send(
        &self,
        request: Request,
    ) -> BoxFuture<'_, Result<Response, pubky_dispatch_client::TransportError>> {
        use pubky_dispatch_core::ops::{Entry, OpBody, OpKind, OpsRequest};

        if self.refusing.load(Ordering::Relaxed)
            && request.method == "POST"
            && request.path_and_query.ends_with("/ops")
            && OpsRequest::parse(&request.body)
                .unwrap()
                .entries
                .iter()
                .any(|entry| {
                    let Entry::Signed(signed) = entry else {
                        return false;
                    };
                    let op = OpBody::parse(&signed.body_bytes().unwrap()).unwrap();
                    matches!(op.kind, OpKind::RecordPut(put) if put.key == self.record)
                })
        {
            self.service.fail_next(Fault::Error {
                path: "/ops".into(),
                code: pubky_dispatch_core::http::ErrorCode::PermissionMissing,
                current: serde_json::Value::Null,
            });
        }
        self.router.send(request)
    }
}

async fn restore_before_placement(
    w: &World,
    who: &TestSigner,
    backup: &PaykitBackup,
    record: [u8; 32],
) -> (Paykit, Arc<RefusePlacement>) {
    let placement = Arc::new(RefusePlacement {
        router: w.router.clone(),
        service: who.dispatch().clone(),
        record,
        refusing: AtomicBool::new(true),
    });
    let mut env = w.env(who, store());
    env.transport = placement.clone();
    let mut app = Box::pin(Paykit::restore(
        env,
        PaykitConfig::default(),
        backup,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    read_restored_client(&mut app).await;
    (app, placement)
}

async fn assert_held_for_placement(app: &mut Paykit, stop: &str, kept_key: &[u8]) {
    assert!(held(app, stop).await);
    assert_eq!(
        decisions_through_listing(app).await,
        HeldMessages {
            still_held: Some(1),
            ..HeldMessages::default()
        }
    );
    assert!(app
        .app()
        .protocol_get("paykit", kept_key)
        .await
        .unwrap()
        .is_some());
    assert!(held(app, stop).await);
}

async fn assert_stop_withdrawn(app: &mut Paykit, stop: &str) {
    assert_eq!(
        decisions_through_listing(app).await,
        HeldMessages {
            withdrawn: 1,
            ..nothing_held()
        }
    );
    assert!(app.message_delivery(stop).await.unwrap().is_none());
    unknown(app, stop).await;
    assert!(stop_notice_ids(app, stop).await.is_empty());
}

// restore_held.rs:530 waits for kept_unplaced before releasing the recorded rejection.
#[tokio::test]
async fn test_restore_held_kept_payee_cancellation_holds_the_payer_rejection() {
    let w = World::new();
    let (wallet, request, stop, mut shop, backup) = Box::pin(carried_rejection(&w, store())).await;
    let key = wallet
        .app()
        .record_key(&outcome_record::payer_record(&request))
        .unwrap();
    drop(wallet);
    let (mut wallet, placement) =
        Box::pin(restore_before_placement(&w, &w.alice, &backup, key)).await;
    let sale = shop.payee_activity().await.unwrap()[0].record;
    let cancellation = shop.cancel_sent_request(&sale, None).await.unwrap();
    let kept = crate::one_time::stop_key(&w, &wallet, request.request_hash);
    assert_held_for_placement(&mut wallet, &stop, &kept).await;
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let before = ActivityRecord::read(
        held.content.value().unwrap(),
        &w.alice_id(),
        ActivitySide::Payer,
    )
    .unwrap();
    assert_eq!(before.response.unwrap().message().msg_id, stop);
    assert!(before.cancellation.is_none());

    placement.refusing.store(false, Ordering::Relaxed);
    assert_stop_withdrawn(&mut wallet, &stop).await;
    assert!(wallet
        .app()
        .protocol_get("paykit", &kept)
        .await
        .unwrap()
        .is_none());
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let after = ActivityRecord::read(
        held.content.value().unwrap(),
        &w.alice_id(),
        ActivitySide::Payer,
    )
    .unwrap();
    assert_eq!(after.cancellation.unwrap().message().msg_id, cancellation);
}

// allowances/delivery.rs:288 waits for the kept end before releasing our recorded end.
#[tokio::test]
async fn test_restore_held_kept_allowance_end_holds_ours() {
    let w = World::new();
    let (mut original, mut wallet, _exec, _phone, key, mine) =
        Box::pin(allowances::executed(&w)).await;
    original.receive(None).await.unwrap();
    let accepted = allowance_record(&original, &key).await;
    let target = allowances::allowances_target(&w.alice_id(), &accepted.allowance_id);
    original.app_mut().hold_queued(&w.alice_id()).await.unwrap();
    let stop = original.end_allowance(&key).await.unwrap();
    original
        .app_mut()
        .release_queued(&w.alice_id())
        .await
        .unwrap();
    let backup = original.backup().await.unwrap();
    let (mut shop, placement) = Box::pin(restore_before_placement(&w, &w.bob, &backup, key)).await;
    let counterparty_end = wallet.end_allowance(&mine).await.unwrap();
    let kept = format!(
        "ae/{}/{counterparty_end}",
        pubky_dispatch_core::enc::b64(&key)
    );
    assert_held_for_placement(&mut shop, &stop, kept.as_bytes()).await;
    assert_eq!(
        allowance_record(&shop, &key)
            .await
            .end
            .unwrap()
            .message()
            .msg_id,
        stop
    );

    // An Allowance has one end slot. A later write loses our end before the
    // counterparty's kept end is placed, so the restored shop must withdraw ours.
    allowances::write_record(&mut original, &target, Some(&accepted.to_json().unwrap())).await;
    shop.app_mut().read_record_fresh(&target).await.unwrap();
    assert!(allowance_record(&shop, &key).await.end.is_none());
    placement.refusing.store(false, Ordering::Relaxed);
    assert_stop_withdrawn(&mut shop, &stop).await;
    assert!(shop
        .app()
        .protocol_get("paykit", kept.as_bytes())
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        allowance_record(&shop, &key)
            .await
            .end
            .unwrap()
            .message()
            .msg_id,
        counterparty_end
    );
}

// restore_held.rs:404 checks Body::parse before the evidence read can leave a bad end waiting.
#[tokio::test]
async fn test_restore_held_malformed_held_body_is_withdrawn() {
    let w = World::new();
    let mut wallet = w
        .join(
            &w.alice,
            "wallet",
            AppScopes::new().allowance().build().unwrap(),
        )
        .await;
    let mut end = OutgoingMessage::new(w.bob_id(), "paykit.allowance_end", Default::default());
    end.refs = vec![[1; 32], [2; 32]];
    end.body.insert("allowance_id".into(), false.into());
    let stop = wallet.app_mut().queue(end).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    assert!(held(&wallet, &stop).await);
    assert_stop_withdrawn(&mut wallet, &stop).await;
}

// allowances/delivery.rs:276 treats NotFound with no retained record as absent.
#[tokio::test]
async fn test_restore_held_allowance_end_with_no_record_is_withdrawn() {
    use pubky_dispatch_client::app::{KeptReason, RecordRead};

    let w = World::new();
    let mut wallet = w
        .join(
            &w.alice,
            "wallet",
            AppScopes::new().allowance().build().unwrap(),
        )
        .await;
    let id = paykit_lib::AllowanceId::parse("0b4f9c1e-8d2a-4e6b-9f3c-5a7d1e2b8c40").unwrap();
    let target = allowances::allowances_target(&w.bob_id(), &id);
    let body = paykit_lib::bodies::AllowanceReply {
        allowance_id: id,
        executor_app_id: None,
    }
    .to_body(paykit_lib::Kind::AllowanceEnd)
    .unwrap();
    let mut end = OutgoingMessage::new(w.bob_id(), "paykit.allowance_end", body);
    end.refs = vec![[1; 32], [2; 32]];
    let stop = wallet.app_mut().queue(end).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    assert!(held(&wallet, &stop).await);
    assert_stop_withdrawn(&mut wallet, &stop).await;
    assert!(matches!(
        wallet.app_mut().read_record_fresh(&target).await.unwrap(),
        RecordRead::Kept {
            reason: KeptReason::NotFound,
            record: None
        }
    ));
}

async fn shop_on(w: &World, storage: Arc<dyn StateStore>) -> Paykit {
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            storage,
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    shop
}

// outbound.rs:157 calls carried_lists_owed before the outbox can expire an unheld list.
#[tokio::test]
async fn test_restore_held_unheld_carried_list_owes_its_withdrawal_before_flush() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let storage = store();
    let mut shop = shop_on(&w, storage.clone()).await;
    let to = w.alice_id();
    // The outbox is the only evidence of the recipient. No publication can rediscover it.
    let original = queue_unpublished_list(&w, &mut shop, to).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = Box::pin(restore(&w, &w.bob, storage, &backup)).await;
    read_restored_client(&mut shop).await;
    assert!(shop
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .iter()
        .any(|q| q.msg_id == original && !q.held));
    let recipient = format!("lrec/{}", pubky_dispatch_core::enc::b64(&to));
    assert!(shop
        .app()
        .protocol_get("paykit", recipient.as_bytes())
        .await
        .unwrap()
        .is_none());

    w.clock.advance_ms(SENT_RETENTION_MS + 1);
    shop.flush().await.unwrap();
    assert!(shop.message_delivery(&original).await.unwrap().is_none());
    unknown(&mut shop, &original).await;
    assert!(shop
        .app()
        .protocol_get("paykit", recipient.as_bytes())
        .await
        .unwrap()
        .is_some());

    crate::common::finish_listing(&mut shop).await;
    shop.flush().await.unwrap();
    let (fresh, list) = sent_list(&shop, &to).await;
    assert_ne!(fresh, original);
    assert!(list.payment_endpoints.is_empty());
    receive_message(&mut wallet, &fresh).await;
}

// restore_held.rs give_up_expired_held: past its 30 days, while more of the feed keeps the gate closed.
#[tokio::test]
async fn test_restore_held_past_thirty_days_withdrawn_while_the_feed_has_more() {
    use pubky_dispatch_client::app::{AppConfig, FEED_PAGES_PER_READ};

    let w = World::new();
    let (mut wallet, request, stop, _shop, _) = Box::pin(carried_rejection(&w, store())).await;
    let signed_at = w.clock.now_ms();
    // A later verified backup time separates the Dispatch replay deadline from the
    // ordinary stop's deadline, which runs from the signed message's created_at.
    w.clock.advance_ms(crate::common::HOUR_MS);
    wallet
        .app_mut()
        .read_record_fresh(&outcome_record::payer_record(&request))
        .await
        .unwrap();
    assert_eq!(wallet.app().highest_service_time(), Some(w.clock.now_ms()));
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let config = PaykitConfig::default().with_app(AppConfig {
        page_size: 1,
        ..Default::default()
    });
    let mut wallet = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        config,
        &backup,
        w.rail.clone(),
        w.timer(),
    ))
    .await
    .unwrap();
    read_restored_client(&mut wallet).await;
    assert!(held(&wallet, &stop).await);
    let queued_stop = format!("qs/{stop}");
    assert!(wallet
        .app()
        .protocol_get("paykit", queued_stop.as_bytes())
        .await
        .unwrap()
        .is_some());

    let cutoff = signed_at + SENT_RETENTION_MS - pubky_dispatch_core::records::CLOCK_TOLERANCE_MS;
    w.clock.set_ms(cutoff + 1);
    // The records are listed, so ordinary stop expiry runs too. The rest of the feed
    // keeps the restore's decisions closed; the Dispatch deadline is an hour later.
    for _ in 0..=FEED_PAGES_PER_READ {
        w.inject(
            &w.bob,
            "shop",
            &w.alice,
            &[wallet.app_id()],
            "chat.item",
            Vec::new(),
            Default::default(),
        );
    }
    let read = wallet.receive(None).await.unwrap();
    assert!(read.report.more && !read.report.stopped);
    assert!(read.records_unlisted.is_none() && !read.scope_keys_awaited);
    assert!(!wallet.app().listing_owed().await.unwrap());
    // Past its 30 days the restore's own expiry withdraws it while the rest of the feed
    // keeps the decisions closed, before the client's deadline: told once, no stop notice,
    // and the ordinary stop expiry forgets its queued stop.
    assert_eq!(read.held_messages, nothing_held());
    assert!(!held(&wallet, &stop).await);
    assert!(wallet.message_delivery(&stop).await.unwrap().is_none());
    assert!(stop_notice_ids(&wallet, &stop).await.is_empty());
    let duties = wallet.app_mut().pending_duty_events().await.unwrap();
    assert_eq!(
        duties
            .iter()
            .filter(
                |d| matches!(&d.event, AppEvent::DeliveryUnknown { msg_id, .. } if msg_id == &stop)
            )
            .count(),
        1
    );
    assert!(wallet
        .app()
        .protocol_get("paykit", queued_stop.as_bytes())
        .await
        .unwrap()
        .is_none());
}

// protocol.rs:1492 gives the installation's stop notice precedence over the backup's.
#[tokio::test]
async fn test_restore_held_second_in_place_restore_keeps_an_acknowledged_stop_notice() {
    let w = World::new();
    let (wallet, _, stop, backup) = Box::pin(unrecorded_rejection(&w)).await;
    drop(wallet);
    let storage = store();
    let mut wallet = Box::pin(restore(&w, &w.alice, storage.clone(), &backup)).await;
    withdrawn(&mut wallet).await;
    let ids = stop_notice_ids(&wallet, &stop).await;
    let [id] = ids.as_slice() else {
        panic!("expected one stop notice: {ids:?}")
    };
    let before_acknowledgment = wallet.backup().await.unwrap();
    wallet.acknowledge_notice(id).await.unwrap();
    assert!(stop_notice_ids(&wallet, &stop).await.is_empty());
    drop(wallet);

    let fresh = Box::pin(restore(&w, &w.alice, store(), &before_acknowledgment)).await;
    assert_eq!(stop_notice_ids(&fresh, &stop).await, ids);
    drop(fresh);
    let mut wallet = Box::pin(restore(&w, &w.alice, storage, &before_acknowledgment)).await;
    assert!(stop_notice_ids(&wallet, &stop).await.is_empty());
    crate::common::finish_listing(&mut wallet).await;
    assert!(stop_notice_ids(&wallet, &stop).await.is_empty());
}

// publish/withdrawal.rs:204 skips queued.resend before recording ListDeliveryUnknown.
#[tokio::test]
async fn test_restore_held_resent_accepted_list_gets_no_unknown_notice() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let storage = store();
    let mut shop = shop_on(&w, storage.clone()).await;
    let to = w.alice_id();
    shop.set_list_endpoints(publishing::offered("lno1first"))
        .await
        .unwrap();
    shop.publish_list_to(to).await.unwrap();
    let (original, list) = sent_list(&shop, &to).await;
    assert!(!list.payment_endpoints.is_empty() && list.expires_at > w.clock.now_ms());
    receive_message(&mut wallet, &original).await;
    let backup = shop.backup().await.unwrap();
    shop.app_mut().hold_queued(&to).await.unwrap();
    shop.app_mut().resend(&original).await.unwrap();
    shop.app_mut().release_queued(&to).await.unwrap();
    drop(shop);

    let mut shop = Box::pin(restore(&w, &w.bob, storage, &backup)).await;
    read_restored_client(&mut shop).await;
    let resend = shop
        .app()
        .queued_messages("paykit")
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.msg_id == original)
        .unwrap();
    assert!(resend.resend && resend.held);
    let at = format!("lres/{}", pubky_dispatch_core::enc::b64(&to));
    let reservation = shop
        .app()
        .protocol_get("paykit", at.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let reservation: serde_json::Value = serde_json::from_slice(&reservation).unwrap();
    assert_eq!(reservation["d"]["id"], original);
    assert_eq!(reservation["d"]["hash"], serde_json::json!(resend.hash));
    // Inspect before receive can replace the reservation and prune its notices.
    shop.flush().await.unwrap();
    assert!(held(&shop, &original).await);
    let unknown_list = |notices: Vec<paykit_sdk::Notice>| {
        notices.iter().any(|n| {
            n.kind == NoticeKind::ListDeliveryUnknown
                && n.msg_id.as_deref() == Some(original.as_str())
        })
    };
    assert!(!unknown_list(shop.notices().await.unwrap()));
    assert_eq!(decisions_through_listing(&mut shop).await.withdrawn, 1);
    let duties = shop.app_mut().pending_duty_events().await.unwrap();
    assert!(duties.iter().any(|d| matches!(&d.event,
        AppEvent::MessageUndelivered { msg_id, resend: true, .. } if msg_id == &original)));
    assert!(!unknown_list(shop.notices().await.unwrap()));
}
