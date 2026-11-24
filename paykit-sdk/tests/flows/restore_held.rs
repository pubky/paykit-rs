//! Original deliveries carried across a backup and restore (Paykit spec 7.2, 9).

use std::sync::Arc;

use paykit_lib::bodies::PaymentProof;
use paykit_lib::records::ListPublicationRecord;
use paykit_sdk::{
    AllowanceStatus, AppScopes, HeldMessages, NoticeKind, PayOptions, PayOutcome, PayeeStatus,
    Paykit, PaykitBackup, PaykitConfig, Rail, RailOutcome, ReceiptsOf, RequestRef, RequestStatus,
};
use pubky_switch_client::app::{AppEvent, OutgoingMessage, SENT_RETENTION_MS};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;
use pubky_switch_client::testing::{Fault, TestSigner};

use crate::common::{draft, endpoint, receive_message, request_from, store, World};
use crate::{allowances, publishing};

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

async fn released(app: &mut Paykit, id: &str) {
    assert!(held(app, id).await);
    for _ in 0..8 {
        let report = app.receive(None).await.unwrap();
        if report.held_messages.released == 1 {
            break;
        }
        // Paykit decides once its own first read, the listing included, is complete.
        assert!(report.held_messages.still_held > 0);
    }
    assert!(!held(app, id).await);
    app.flush().await.unwrap();
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

#[tokio::test]
async fn test_restore_held_open_request_keeps_stop_notice_across_reload() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let mut rejection = OutgoingMessage::new(
        w.bob_id(),
        "paykit.payment_request_rejection",
        Default::default(),
    );
    rejection.refs.push(request.request_hash);
    let id = wallet.app_mut().queue(rejection).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let storage = store();
    let mut wallet = Box::pin(restore(&w, &w.alice, storage.clone(), &backup)).await;
    assert!(held(&wallet, &id).await);
    withdrawn(&mut wallet).await;
    assert!(!held(&wallet, &id).await);
    let app_id = wallet.app_id();
    drop(wallet);
    let wallet = w
        .load_on_clock(&w.alice, storage, app_id, w.clock.clone())
        .await;
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert!(
        wallet
            .notices()
            .await
            .unwrap()
            .iter()
            .any(|n| n.kind == NoticeKind::StopNotRecorded
                && n.msg_id.as_deref() == Some(id.as_str()))
    );
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
    let sent = shop.app().sent_to(&to, 0, u64::MAX).await.unwrap();
    let lists: Vec<_> = sent
        .iter()
        .filter(|s| s.kind == "paykit.private_payment_list")
        .collect();
    let [empty] = lists.as_slice() else {
        panic!("expected one fresh withdrawal")
    };
    assert_ne!(empty.msg_id, original.msg_id);
    let msg = pubky_switch_core::message::Message::parse(
        empty.plaintext.json(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    let paykit_lib::Body::PrivatePaymentList(list) = paykit_lib::Body::parse(&msg).unwrap() else {
        unreachable!()
    };
    assert!(list.payment_endpoints.is_empty());
    assert!(list.version.counter() > original.counter);
    receive_message(&mut wallet, &empty.msg_id).await;
}

/// Alice pays bob's request; the backup carries the finished payment's queued proof.
async fn carried_proof(w: &World) -> (Paykit, Paykit, RequestRef, String, PaykitBackup) {
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
    let PayOutcome::Paid {
        proof_msg_id: Some(proof),
        ..
    } = outcome
    else {
        panic!("expected a finished payment: {outcome:?}")
    };
    wallet.app_mut().release_queued(&w.bob_id()).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    (wallet, shop, request, proof, backup)
}

#[tokio::test]
async fn test_restore_held_proof_of_a_finished_payment_is_released_and_credited_once() {
    let w = World::new();
    let (wallet, mut shop, _request, proof, backup) = Box::pin(carried_proof(&w)).await;
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
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_restore_held_proof_withdrawn_once_its_receipt_is_recorded() {
    let w = World::new();
    let (mut wallet, mut shop, _request, proof, backup) = Box::pin(carried_proof(&w)).await;
    // After the backup the proof goes, the payee credits it and the receipt is recorded.
    wallet.flush().await.unwrap();
    receive_message(&mut shop, &proof).await;
    let sale = shop.payee_activity().await.unwrap()[0].record;
    let receipt = shop.confirm_payment(&sale).await.unwrap().unwrap();
    receive_message(&mut wallet, &receipt).await;
    drop(wallet);
    let mut wallet = Box::pin(restore(&w, &w.alice, store(), &backup)).await;
    let mut counts = HeldMessages::default();
    for _ in 0..8 {
        let report = wallet.receive(None).await.unwrap();
        counts.released += report.held_messages.released;
        counts.withdrawn += report.held_messages.withdrawn;
        if !report.report.more && report.records_unlisted.is_none() && counts.withdrawn > 0 {
            break;
        }
    }
    // A receipt for its attempt leaves nothing owed (Paykit spec 7.2); the payment's
    // acceptance, carried beside it, is withdrawn as every message but a stop or proof.
    assert_eq!((counts.released, counts.withdrawn), (0, 2), "{counts:?}");
    assert!(!held(&wallet, &proof).await);
    assert_eq!(w.rail.all_executions().len(), 1);
}

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
    assert_eq!(w.rail.all_executions().len(), 1);
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
    let mut rejection = OutgoingMessage::new(
        w.bob_id(),
        "paykit.payment_request_rejection",
        Default::default(),
    );
    rejection.refs.push(request.request_hash);
    let id = wallet.app_mut().queue(rejection).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    phone.accept_subscription(&request).await.unwrap();
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
    // The subscription stays open: the user is told the record no longer shows the
    // rejection, and may cancel it (Paykit spec 7.2).
    assert!(
        wallet
            .notices()
            .await
            .unwrap()
            .iter()
            .any(|n| n.kind == NoticeKind::StopNotRecorded
                && n.msg_id.as_deref() == Some(id.as_str()))
    );
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
