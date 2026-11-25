//! Original deliveries carried across a backup and restore (Paykit spec 7.2, 9).

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
use pubky_switch_client::app::{AppError, AppEvent, OutgoingMessage, SENT_RETENTION_MS};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::error::StoreError;
use pubky_switch_client::store::{Fault as StoreFault, FaultStore, StateStore};
use pubky_switch_client::testing::{Fault, TestSigner};

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
    let mut counts = HeldMessages::default();
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
        assert!(report.held_messages.still_held > 0);
    }
    panic!("the held message {id} was never released");
}

async fn cancellation(w: &World) -> (Paykit, RequestRef, String, Paykit) {
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
        HeldMessages::default()
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
async fn test_restore_held_unreadable_stop_waits_then_releases() {
    let w = World::new();
    let (mut wallet, request, stop, mut shop) = Box::pin(cancellation(&w)).await;
    let key = shop
        .app()
        .record_key(&crate::restore::payee_record(&request))
        .unwrap();
    w.bob.switch().fail_next(Fault::Unreachable {
        path: format!("records/{}", pubky_switch_core::enc::b64(&key)),
        after: 0,
    });
    let counts = shop.receive(None).await.unwrap().held_messages;
    assert_eq!(
        (counts.released, counts.withdrawn, counts.still_held),
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
async fn test_restore_held_past_deadline_is_given_up_by_switch() {
    let w = World::new();
    let (mut wallet, _, stop, mut shop) = Box::pin(cancellation(&w)).await;
    assert!(held(&shop, &stop).await);
    w.clock.advance_ms(SENT_RETENTION_MS + 1);
    assert_eq!(
        shop.receive(None).await.unwrap().held_messages,
        HeldMessages::default()
    );
    unknown(&mut shop, &stop).await;
    assert!(!wallet.receive(None).await.unwrap().applied.contains(&stop));
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
            &[pubky_switch_client::app::LocalWrite::Delete {
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
    let msg = pubky_switch_core::message::Message::parse(
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
    let acceptance =
        |m: &&pubky_switch_client::app::SentMessage| m.kind == "paykit.payment_request_acceptance";
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
    // the resume signs no replacement (Switch spec 12.4, Paykit spec 7.2).
    assert!(matches!(
        wallet.app().delivery(original).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
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
    let mut counts = HeldMessages::default();
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
        (counts.released, counts.withdrawn, counts.still_held),
        (0, 0, 1),
        "{counts:?}"
    );
    assert!(held(&wallet, &id).await);
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    Box::pin(released(&mut wallet, &id)).await;
}

/// The reply grants for `peer` written at `who`'s switch.
fn reply_grants(who: &TestSigner, peer: [u8; 32]) -> usize {
    use pubky_switch_core::admission::PeerReason;
    use pubky_switch_core::ops::OpKind;
    who.switch()
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
        .switch()
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
    let before = w.alice.switch().entries_from(&w.bob_id());
    let counts = decisions_through_listing(&mut shop).await;
    assert_eq!(
        (counts.released, counts.withdrawn, counts.still_held),
        (0, 1, 0)
    );
    shop.flush().await.unwrap();
    let read = wallet.receive(None).await.unwrap();
    assert!(!allowances::received(&read, "paykit.allowance_end"));
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), before);
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
        (counts.released, counts.withdrawn, counts.still_held),
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
        (counts.released, counts.withdrawn, counts.still_held),
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
        .switch()
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
        .switch()
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
        (counts.released, counts.withdrawn, counts.still_held),
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

/// Fail the Switch commit that records this specific delivery as given up.
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
        (counts.released, counts.withdrawn, counts.still_held),
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

// restore_held.rs:302 saves the release before the following withdrawal can fail.
// paykit.rs:1430 takes that count only when returning a summary.
#[tokio::test]
async fn test_restore_held_release_count_survives_a_later_failed_withdrawal() {
    let w = World::new();
    let (mut wallet, _, stop, _shop, _) = Box::pin(carried_rejection(&w, store())).await;
    w.clock.advance_ms(1);
    let ask = queue_list_request(&mut wallet, w.bob_id()).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let storage = Arc::new(FaultStore::new(store(), StoreFault::None));
    let mut wallet = Box::pin(restore(&w, &w.alice, storage.clone(), &backup)).await;
    assert!(held(&wallet, &stop).await);
    fail_withdrawal(&mut wallet, &storage, &ask).await;
    assert!(!held(&wallet, &stop).await);
    assert!(wallet.message_delivery(&stop).await.unwrap().is_some());
    let counts = wallet.receive(None).await.unwrap().held_messages;
    assert_eq!(
        (counts.released, counts.withdrawn, counts.still_held),
        (1, 1, 0)
    );
    assert_eq!(
        wallet.receive(None).await.unwrap().held_messages,
        HeldMessages::default()
    );
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
    withdrawn(&mut wallet).await;
    assert!(!held(&wallet, &stop).await);
    unknown(&mut wallet, &stop).await;
    assert!(stop_notice_ids(&wallet, &stop).await.is_empty());
    let told = wallet.receive(None).await.unwrap();
    assert!(told.report.events.iter().any(|e| matches!(e,
        AppEvent::DeliveryUnknown { msg_id, .. } if msg_id == &stop)));
    let ids = stop_notice_ids(&wallet, &stop).await;
    let [id] = ids.as_slice() else {
        panic!("expected one stop notice: {ids:?}")
    };
    wallet.acknowledge_notice(id).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(stop_notice_ids(&wallet, &stop).await.is_empty());
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
    wallet.flush().await.unwrap();
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/feed".into(),
        after: 0,
    });
    assert!(wallet.receive(None).await.is_err());
    assert!(held(&wallet, &stop).await);
    assert!(!shop.receive(None).await.unwrap().applied.contains(&stop));
    let read = wallet.receive(None).await.unwrap();
    assert!(read.report.held_cursor.is_some());
    assert_eq!(read.held_messages.released, 1);
    wallet.flush().await.unwrap();
    receive_message(&mut shop, &stop).await;
}
