//! A provider loss (Paykit spec 7.3, "After a provider loss"; Pubky Switch spec 12.6): the
//! signer declares alice's generation lost at a new service, the old one stops answering,
//! and her apps follow the loss there.

use std::sync::Arc;

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    Doubtful, LossBoundary, PayOptions, PayOutcome, Paykit, PaykitBackup, PaykitConfig,
    PaykitError, PaymentDoubt, Rail, RailDeclaration, Refusal, RequestRef, RequestStatus,
};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::{ScriptedSwitch, SWITCH_ORIGIN};
use pubky_switch_core::claims::ClaimStatus;
use pubky_switch_core::records::{Inbox, CLOCK_TOLERANCE_MS};

use crate::common::{draft, request_from, store, World, ALICE_ORIGIN};
use crate::restore::{is_refused, layer_of, nothing_restored, refused_restore, with_layer};

/// The new service's key secret.
const NEXT_SECRET: [u8; 32] = [0x4e; 32];

/// The key secret of the service after a second loss.
const LATER_SECRET: [u8; 32] = [0x4f; 32];

/// Alice's signer declares her current generation lost (Switch spec 12.6 step 1): a new
/// service recovering the account at the next generation, routed at [`SWITCH_ORIGIN`], the
/// next `identity.json` naming it with `recovery_from`, delivered there and published; then
/// the old service is unreachable.
fn declare_loss(w: &World) {
    declare_loss_of(w, ALICE_ORIGIN, NEXT_SECRET);
}

/// [`declare_loss`] of the service at `lost_origin`, the new one under `secret`.
fn declare_loss_of(w: &World, lost_origin: &str, secret: [u8; 32]) -> Arc<ScriptedSwitch> {
    let lost_generation = w
        .homeserver
        .identity_doc(&w.alice_id())
        .unwrap()
        .generation();
    let next = ScriptedSwitch::recovering(
        w.alice.identity().key(),
        secret,
        Arc::clone(&w.clock),
        lost_generation + 1,
        lost_generation,
    )
    .unwrap();
    let doc = w
        .alice
        .sign_next(|body| {
            body.generation = lost_generation + 1;
            body.inbox = Inbox::new(SWITCH_ORIGIN, next.service_key());
            body.recovery_from = Some(lost_generation);
        })
        .unwrap();
    next.deliver_identity(&doc).unwrap();
    w.router.remove(lost_origin);
    w.router.add_switch(SWITCH_ORIGIN, &next);
    next
}

/// `app` follows the loss: its own `identity.json` from the Homeserver, then reads at the new
/// service until its steps of the recovery have run, and its reconciliation ends. Returns the
/// end of the loss's residual window.
async fn follow_loss(app: &mut Paykit) -> u64 {
    app.app_mut().refresh_identity().await.unwrap();
    for _ in 0..4 {
        app.receive(None).await.unwrap();
    }
    let info = app.app().recovery_info().await.unwrap().unwrap();
    assert!(info.steps_done, "{info:?}");
    assert!(info.loss.uncertain_until_ms > info.loss.declared_at_ms);
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    info.loss.uncertain_until_ms
}

/// The payer's activity for this request, as both of Alice's apps address it.
fn request_target(request: &RequestRef) -> pubky_switch_client::app::RecordTarget {
    use paykit_lib::collections::Collection;
    activity_target(request, Collection::PayerActivity)
}

fn activity_target(
    request: &RequestRef,
    family: paykit_lib::collections::Collection,
) -> pubky_switch_client::app::RecordTarget {
    use paykit_lib::derive::natural_key;
    use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
    RecordTarget::new(
        SwitchCollection::new(family.name(), family.fixed_policy()),
        &natural_key::request_record(&request.payee, &request.request_hash),
    )
}

/// The phone's signed write was in flight when the provider was lost. The wallet's
/// write on the same base was applied there. The phone's copy reaches recovery first.
async fn recover_activity_with_phone_write(
    w: &World,
    wallet: &mut Paykit,
    phone: &mut Paykit,
    target: &pubky_switch_client::app::RecordTarget,
    delete_held: bool,
) -> Arc<ScriptedSwitch> {
    use pubky_switch_client::client::answers::{RecoveryUpload, UploadedOp};
    use pubky_switch_client::client::{Credentials, SwitchClient};
    use pubky_switch_client::testing::Fault;
    use pubky_switch_core::ops::{Entry, OpsRequest};
    let key = wallet.app().record_key(target).unwrap();
    let current = wallet.app_mut().read_record(target).await.unwrap().unwrap();
    let value = current.content.value().unwrap();
    let base = wallet
        .app_mut()
        .put_record(target, value, vec![])
        .await
        .unwrap();
    phone.app_mut().read_record(target).await.unwrap().unwrap();
    let mut manual: serde_json::Value = serde_json::from_slice(value).unwrap();
    if target.collection.name == paykit_lib::collections::Collection::PayerActivity.name() {
        manual["manual_only"] = true.into();
    }
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    assert!(phone
        .app_mut()
        .put_record(target, &serde_json::to_vec(&manual).unwrap(), vec![])
        .await
        .is_err());
    let missed = w.alice.switch().unreached_bodies().pop().unwrap();
    let op = OpsRequest::parse(&missed)
        .unwrap()
        .entries
        .into_iter()
        .find_map(|entry| {
            let Entry::Signed(op) = entry else { return None };
            let verified = pubky_switch_core::ops::SignedOp::verify_with_cert(
                &op, phone.app().cert(), &w.alice_id(),
            ).unwrap();
            match &verified.body().kind {
                pubky_switch_core::ops::OpKind::RecordPut(write) if write.key == key => Some(op),
                _ => None,
            }
        })
        .expect("the phone's signed write of this record");
    let applied = if delete_held {
        wallet
            .app_mut()
            .delete_record(target, vec![])
            .await
            .unwrap()
    } else {
        wallet
            .app_mut()
            .put_record(target, value, vec![])
            .await
            .unwrap()
    };
    assert_eq!(applied.version.version, base.version.version + 1);
    let lost = wallet.app().own_identity().signed().clone();
    let next = declare_loss_of(w, ALICE_ORIGIN, NEXT_SECRET);
    phone.app_mut().refresh_identity().await.unwrap();
    phone.app_mut().follow_provider().await.unwrap();
    let client = SwitchClient::new(
        SWITCH_ORIGIN,
        w.alice_id(),
        Credentials::App {
            identity: w.alice_id(),
            keys: w.alice.identity().app_keys("phone"),
            cert: phone.app().cert().clone(),
        },
        Some(next.service_key()),
        w.router.clone(),
        w.clock.clone(),
    )
    .unwrap();
    client
        .recovery_upload(
            &RecoveryUpload {
                lost_identity: Some(lost),
                records: vec![UploadedOp {
                    op,
                    cert: phone.app().cert().signed().clone(),
                    proof: None,
                }],
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
    wallet.app_mut().refresh_identity().await.unwrap();
    wallet.app_mut().follow_provider().await.unwrap();
    let held = wallet.app_mut().read_record(target).await.unwrap().unwrap();
    assert_eq!(held.version, applied.version);
    assert!(held.different_write_at_held_version);
    assert_eq!(next.record(&key).unwrap().state.version, held.version);
    next
}

#[tokio::test]
async fn test_provider_loss_activity_with_another_write_at_its_version_survives_reload() {
    let w = World::new();
    let disk = store();
    let mut wallet = w.wallet_on("wallet", disk.clone()).await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    // The receive step has opened the one-time request and queued its floor check.
    // The host has not yet run the SDK pass that acts on that obligation.
    wallet.app_mut().receive(None).await.unwrap();
    let owed_floor = wallet.app().protocol_scan("paykit", b"fo/").await.unwrap();
    assert_eq!(owed_floor.len(), 1);
    let target = request_target(&request);
    let next = recover_activity_with_phone_write(&w, &mut wallet, &mut phone, &target, false).await;
    let key = wallet.app().record_key(&target).unwrap();
    let id = wallet.app_id();
    drop(wallet);
    let mut wallet = w.load_on_clock(&w.alice, disk, id, w.clock.clone()).await;
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert!(held.different_write_at_held_version);
    assert_eq!(next.record(&key).unwrap().state.version, held.version);
    follow_loss(&mut wallet).await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(reason)) if reason.contains("another write at the held activity")),
        "{refused:?}"
    );
    assert_eq!(executions(&w), 0);
    assert_eq!(
        wallet.app().protocol_scan("paykit", b"fo/").await.unwrap(),
        owed_floor
    );
    accept_activity_successor(&mut wallet, &mut phone, &target).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"fo/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_provider_loss_receipt_waits_for_an_accepted_activity_successor() {
    disputed_receipt(false).await;
}

#[tokio::test]
async fn test_provider_loss_disputed_receipt_expires_with_its_notice() {
    disputed_receipt(true).await;
}

async fn disputed_receipt(expires: bool) {
    let w = World::new();
    let disk = store();
    let mut wallet = w.wallet_on("wallet", disk.clone()).await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }));
    shop.receive(None).await.unwrap();
    let sale = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.request_hash == Some(request.request_hash))
        .unwrap()
        .record;
    let target = request_target(&request);
    let next = recover_activity_with_phone_write(&w, &mut wallet, &mut phone, &target, false).await;
    let key = wallet.app().record_key(&target).unwrap();
    follow_loss(&mut wallet).await;
    if let Some(shown) = wallet
        .divergent_records()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.record == key)
    {
        wallet
            .confirm_divergent(&shown, UserConfirmed::new())
            .await
            .unwrap();
    }
    assert!(
        wallet
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .different_write_at_held_version
    );
    w.clock.advance_ms(11 * 60_000);
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let before = next.record(&key).unwrap().revision;
    let receipt = shop.confirm_payment(&sale).await.unwrap().unwrap();
    let delivery = shop.app_mut().flush_message(&receipt).await;
    let read = wallet.receive(None).await.unwrap();
    let kept = wallet.app().protocol_scan("paykit", b"pr/").await.unwrap();
    assert_eq!(
        kept.len(),
        1,
        "keep the original receipt: {delivery:?} {read:?}"
    );
    assert!(kept[0].0.ends_with(receipt.as_bytes()));
    assert_eq!(next.record(&key).unwrap().revision, before);
    let id = wallet.app_id();
    drop(wallet);
    let mut wallet = w.load_on_clock(&w.alice, disk, id, w.clock.clone()).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.app().protocol_scan("paykit", b"pr/").await.unwrap(),
        kept
    );
    let other = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.request(&other).await.unwrap().request, other);
    assert_eq!(
        wallet.app().protocol_scan("paykit", b"pr/").await.unwrap(),
        kept
    );

    if expires {
        w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - 1);
        wallet.receive(None).await.unwrap();
        assert_eq!(
            wallet.app().protocol_scan("paykit", b"pr/").await.unwrap(),
            kept
        );
        w.clock.advance_ms(1);
        wallet.receive(None).await.unwrap();
        assert!(wallet
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .is_empty());
        let given_up = wallet
            .notices()
            .await
            .unwrap()
            .into_iter()
            .filter(|n| n.msg_id.as_deref() == Some(&receipt))
            .collect::<Vec<_>>();
        assert_eq!(given_up.len(), 1);
        assert_eq!(given_up[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
        wallet.receive(None).await.unwrap();
        assert_eq!(
            wallet
                .notices()
                .await
                .unwrap()
                .into_iter()
                .filter(|n| n.msg_id.as_deref() == Some(&receipt))
                .count(),
            1
        );
        assert_eq!(next.record(&key).unwrap().revision, before);
        return;
    }
    accept_activity_successor(&mut wallet, &mut phone, &target).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    assert!(wallet.request(&request).await.unwrap().receipt.is_some());
    assert_eq!(executions(&w), 1);
}

/// The phone writes a successor preserving the accepted activity and its restriction.
async fn accept_activity_successor(
    wallet: &mut Paykit,
    phone: &mut Paykit,
    target: &pubky_switch_client::app::RecordTarget,
) {
    let current = phone.app_mut().read_record(target).await.unwrap().unwrap();
    assert!(!current.different_write_at_held_version);
    let content = current.content.value().unwrap();
    if target.collection.name == paykit_lib::collections::Collection::PayerActivity.name() {
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(content).unwrap()["manual_only"],
            true
        );
    }
    phone
        .app_mut()
        .put_record(target, content, vec![])
        .await
        .unwrap();
    let successor = wallet.app_mut().read_record(target).await.unwrap().unwrap();
    assert!(!successor.different_write_at_held_version);
}

#[tokio::test]
async fn test_provider_loss_completed_rail_waits_for_an_accepted_activity_successor() {
    let w = World::new();
    let disk = store();
    let mut wallet = w.wallet_on("wallet", disk.clone()).await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = paid else {
        panic!("pending: {paid:?}")
    };
    let target = request_target(&request);
    let next = recover_activity_with_phone_write(&w, &mut wallet, &mut phone, &target, false).await;
    let key = wallet.app().record_key(&target).unwrap();
    follow_loss(&mut wallet).await;
    if let Some(shown) = wallet
        .divergent_records()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.record == key)
    {
        wallet
            .confirm_divergent(&shown, UserConfirmed::new())
            .await
            .unwrap();
    }
    let before = next.record(&key).unwrap().revision;
    w.rail.settle(&attempt_id, true);
    w.rail.set_declaration(
        RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true)
            .with_history(true),
    );
    let held = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(held.as_slice(), [PayOutcome::RecordHeld { .. }]),
        "{held:?}"
    );
    assert_eq!(next.record(&key).unwrap().revision, before);
    let id = wallet.app_id();
    drop(wallet);
    let mut wallet = w.load_on_clock(&w.alice, disk, id, w.clock.clone()).await;
    let held = wallet.repair_activity_record(&attempt_id).await.unwrap();
    assert!(matches!(held, PayOutcome::RecordHeld { .. }), "{held:?}");
    assert_eq!(next.record(&key).unwrap().revision, before);
    assert_eq!(w.rail.executions(&attempt_id), 1);
    accept_activity_successor(&mut wallet, &mut phone, &target).await;
    let finished = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(finished.as_slice(), [PayOutcome::Paid { .. }]),
        "{finished:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_provider_loss_restore_first_reads_the_current_activity() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    let target = request_target(&request);
    let key = wallet.app().record_key(&target).unwrap();
    recover_activity_with_phone_write(&w, &mut wallet, &mut phone, &target, false).await;
    drop(wallet);
    let mut wallet = restored(&w, &backup).await;
    assert!(wallet.app().shared_record(&key).await.unwrap().is_none());
    wallet.app_mut().refresh_identity().await.unwrap();
    wallet.receive(None).await.unwrap();
    let first = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert!(!first.different_write_at_held_version);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(first.content.value().unwrap()).unwrap()
            ["manual_only"],
        true
    );
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_disputed_delete_protects_its_cancellation_from_expiry_and_eviction() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let target = request_target(&request);
    let next = recover_activity_with_phone_write(&w, &mut wallet, &mut phone, &target, true).await;
    let record = wallet.app().record_key(&target).unwrap();
    let held = wallet.app().shared_record(&record).await.unwrap().unwrap();
    assert_eq!(
        held.content,
        pubky_switch_client::app::RecordContent::Deleted
    );
    follow_loss(&mut wallet).await;
    let cancel = |hash| {
        let body = paykit_lib::bodies::RequestReply {
            reason: None,
            label: None,
        }
        .to_body()
        .unwrap();
        let signed = w.sign_as(
            &w.bob,
            "shop",
            &w.alice,
            "paykit.payment_request_cancellation",
            vec![hash],
            body,
        );
        next.send_plaintext(w.bob_id(), shop.app().cert(), &signed, &[wallet.app_id()])
            .unwrap();
    };
    cancel(request.request_hash);
    let stop = format!("kc/{}", pubky_switch_core::enc::b64(&record)).into_bytes();
    for i in 0..=paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER {
        w.clock.advance_ms(1);
        cancel([u8::try_from(i + 1).unwrap(); 32]);
    }
    wallet.receive(None).await.unwrap();
    let kept = wallet.app().protocol_scan("paykit", b"kc/").await.unwrap();
    assert!(
        kept.iter().any(|(key, _)| key == &stop),
        "the stop stays outside the capacity limit"
    );
    assert_eq!(kept.len(), paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER + 1);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    wallet.receive(None).await.unwrap();
    let kept = wallet.app().protocol_scan("paykit", b"kc/").await.unwrap();
    assert_eq!(
        kept.iter().map(|(key, _)| key).collect::<Vec<_>>(),
        [&stop],
        "the stop survives ordinary expiry"
    );
    accept_activity_successor(&mut wallet, &mut phone, &target).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
}

#[tokio::test]
async fn test_provider_loss_disputed_request_keeps_its_known_payers_proof() {
    let w = World::new();
    let lists = Arc::new(crate::common::ListsStore::default());
    let seller_scopes = paykit_sdk::AppScopes::new()
        .request()
        .manage_contacts()
        .build()
        .unwrap();
    let mut shop = w
        .join_with(
            &w.alice,
            "shop",
            seller_scopes.clone(),
            PaykitConfig::default(),
            lists.clone(),
        )
        .await;
    let mut phone = w.join(&w.alice, "phone", seller_scopes).await;
    let mut buyer = w
        .join(
            &w.bob,
            "buyer",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    buyer.app_mut().invite(&w.alice_id()).await.unwrap();
    let request = request_from(&mut shop, w.bob_id(), draft("1", w.clock.now_ms())).await;
    buyer.receive(None).await.unwrap();
    let paid = buyer
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid {
        proof_msg_id: Some(proof),
        ..
    } = paid
    else {
        panic!("{paid:?}")
    };
    let payer = buyer
        .app_mut()
        .read_record(&request_target(&request))
        .await
        .unwrap()
        .unwrap();
    let value = paykit_lib::records::ActivityRecord::read(
        payer.content.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payer,
    )
    .unwrap();
    let target = activity_target(&request, paykit_lib::collections::Collection::PayeeActivity);
    let next = recover_activity_with_phone_write(&w, &mut shop, &mut phone, &target, false).await;
    follow_loss(&mut shop).await;
    lists.flag(&w.bob_id());
    next.send_plaintext(
        w.bob_id(),
        buyer.app().cert(),
        value.proof.unwrap().plaintext(),
        &[shop.app_id()],
    )
    .unwrap();
    shop.receive(None).await.unwrap();
    let kept = shop.app().protocol_scan("paykit", b"pr/").await.unwrap();
    assert!(
        kept.iter().any(|(key, _)| key.ends_with(proof.as_bytes())),
        "keep the original proof from the signed request's recipient"
    );
    assert_eq!(shop.spam_count(&w.bob_id()).await.unwrap(), 0);
    accept_activity_successor(&mut shop, &mut phone, &target).await;
    shop.receive(None).await.unwrap();
    let sale = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.request_hash == Some(request.request_hash))
        .unwrap();
    assert_eq!(sale.status, paykit_sdk::PayeeStatus::ProofReceived);
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
}

/// The doubt a payment was refused with, as [`Refusal::MayBePaid`] shows it.
fn doubt_of(paid: &paykit_sdk::Result<PayOutcome>) -> PaymentDoubt {
    match paid.as_ref().err().and_then(PaykitError::refusal) {
        Some(Refusal::MayBePaid {
            doubt: Some(doubt), ..
        }) => (**doubt).clone(),
        _ => panic!("not refused as may be paid: {paid:?}"),
    }
}

/// The provider loss `app` keeps (`p/pl`).
async fn kept_loss(app: &Paykit) -> LossBoundary {
    let kept: serde_json::Value = serde_json::from_slice(
        &app.app()
            .protocol_get("paykit", b"pl")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    serde_json::from_value(kept["d"].clone()).unwrap()
}

/// A provider loss no app followed.
fn some_loss() -> LossBoundary {
    serde_json::from_value(serde_json::json!({
        "generation": 2,
        "declared_at_ms": 1,
        "uncertain_until_ms": 2,
    }))
    .unwrap()
}

/// The generation of the loss `doubt` names.
fn shown_generation(doubt: &PaymentDoubt) -> u64 {
    doubt.before_loss.unwrap().generation()
}

/// Every payment the method carried out, repeats included.
fn executions(w: &World) -> u64 {
    w.rail.all_executions().values().sum()
}

async fn status(app: &Paykit, request: &RequestRef) -> RequestStatus {
    app.request(request).await.unwrap().status
}

#[tokio::test]
async fn test_provider_loss_request_paid_at_the_old_service_needs_confirmation_before_paying_again()
{
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    // The phone pays the request at the old service, and its claim there is SETTLED. The
    // wallet reads nothing after it, and bob's shop receives nothing: no receipt comes.
    let paid = phone
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let obligation = phone.payments().await.unwrap().remove(0).obligation;
    let at_old = phone
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(at_old.view.status, ClaimStatus::Settled);
    assert_eq!(executions(&w), 1);
    // The phone is gone with its state: nothing it held reaches the new service.
    drop(phone);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    // The new service holds no claim for the obligation: nothing settled it there.
    assert!(wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none_or(|read| read.view.status != ClaimStatus::Settled));
    // The request was payable before the declaration: paying it by hand needs the user's
    // confirmation that another app may already have paid it (Switch spec 12.6 step 5).
    let again = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    let doubt = doubt_of(&again);
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    assert_eq!(doubt.obligation, obligation);
    assert_eq!(executions(&w), 1);
    // The user has seen it and pays anyway.
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 2);
}

#[tokio::test]
async fn test_provider_loss_own_attempt_before_the_loss_stays_in_doubt() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // This app attempts the request at the old service, and the method reports it failed:
    // the claim there is FREE again, its attempt `failed`.
    w.rail.next(Behaviour::Fail);
    let failed = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    assert!(!matches!(failed, Ok(PayOutcome::Paid { .. })), "{failed:?}");
    assert_eq!(executions(&w), 0);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    // Another own app may have paid it at the old service meanwhile: this app's own attempt
    // there settles nothing about that.
    let again = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    assert!(doubt_of(&again).before_loss.is_some());
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_automatic_payment_of_an_obligation_from_before_is_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let dispatched = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let held = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // A dispatch before the loss whose answer is lost: the method paid it, and the attempt
    // stays open for a resume to repeat.
    w.rail.set_idempotent(true);
    w.rail.next(Behaviour::LoseAnswer);
    let lost = wallet
        .pay_request(&dispatched, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(!matches!(lost, PayOutcome::Paid { .. }), "{lost:?}");
    assert_eq!(executions(&w), 1);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    // A resume never repeats the dispatch made before the loss.
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        !resumed
            .iter()
            .any(|outcome| matches!(outcome, PayOutcome::Paid { .. })),
        "{resumed:?}"
    );
    assert_eq!(executions(&w), 1);
    // An automatic payment of a request from before the loss is refused, a confirmation of
    // its doubt passing nothing.
    let refused = wallet
        .pay_request_automatically(&held, PayOptions::default())
        .await
        .outcome;
    let doubt = doubt_of(&refused);
    assert!(doubt.before_loss.is_some(), "{doubt:?}");
    let refused = wallet
        .pay_request_automatically(
            &held,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .outcome;
    assert!(doubt_of(&refused).before_loss.is_some());
    assert_eq!(executions(&w), 1);
}

/// Alice's wallet restored from `backup` on a store of its own, the device the backup came
/// from gone.
async fn restored(w: &World, backup: &PaykitBackup) -> Paykit {
    Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap(),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn test_provider_loss_doubt_survives_a_restore_from_a_backup_after_the_loss() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    app.receive(None).await.unwrap();
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    // The restore holds the request until the user has seen the payee may have cancelled
    // it; the loss's doubt comes after that, carried by the backup.
    let options = crate::common::over_hold(&app, &request).await;
    let again = app.pay_request(&request, options).await.outcome;
    assert!(doubt_of(&again).before_loss.is_some(), "{again:?}");
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_restore_from_a_backup_before_the_loss_learns_it_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut laptop = w.wallet("laptop").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    laptop.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    // The laptop follows the loss: the request's record reaches the new service.
    follow_loss(&mut laptop).await;
    // Restored from a backup taken before the loss, the app learns of it when it follows it.
    let mut app = restored(&w, &backup).await;
    assert!(app
        .app()
        .protocol_get("paykit", b"pl")
        .await
        .unwrap()
        .is_none());
    follow_loss(&mut app).await;
    let kept: serde_json::Value = serde_json::from_slice(
        &app.app()
            .protocol_get("paykit", b"pl")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let loss = app.app().recovery_info().await.unwrap().unwrap().loss;
    assert_eq!(kept["d"]["declared_at_ms"], loss.declared_at_ms);
    assert_eq!(kept["d"]["uncertain_until_ms"], loss.uncertain_until_ms);
    app.receive(None).await.unwrap();
    let options = crate::common::over_hold(&app, &request).await;
    let again = app.pay_request(&request, options).await.outcome;
    assert!(doubt_of(&again).before_loss.is_some(), "{again:?}");
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_doubt_outlasts_the_residual_window() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let before = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    let uncertain_until_ms = follow_loss(&mut wallet).await;
    // Hours past the residual window, the request still payable: the window decides which
    // obligations are uncertain, not how long the doubt lasts.
    let now = w.clock.now_ms();
    w.clock
        .advance_ms(uncertain_until_ms.saturating_sub(now) + 6 * 3_600_000);
    let after = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let again = wallet
        .pay_request(&before, PayOptions::default())
        .await
        .outcome;
    assert!(doubt_of(&again).before_loss.is_some(), "{again:?}");
    assert_eq!(executions(&w), 0);
    // A request created after the declaration plus the clock tolerance isn't uncertain.
    assert_eq!(status(&wallet, &after).await, RequestStatus::Open);
    let paid = wallet
        .pay_request(&after, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_confirm_not_paid_before_loss_clears_that_obligation_only() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let first_doubt = doubt_of(
        &wallet
            .pay_request(&first, PayOptions::default())
            .await
            .outcome,
    );
    let second_doubt = doubt_of(
        &wallet
            .pay_request(&second, PayOptions::default())
            .await
            .outcome,
    );
    // A payment the user confirms over the doubt that fails states nothing: the second
    // request stays in doubt.
    w.rail.next(Behaviour::Fail);
    let failed = wallet
        .pay_request(
            &second,
            PayOptions::default().with_may_be_paid_confirmed(second_doubt),
        )
        .await
        .outcome;
    assert!(!matches!(failed, Ok(PayOutcome::Paid { .. })), "{failed:?}");
    assert!(doubt_of(
        &wallet
            .pay_request(&second, PayOptions::default())
            .await
            .outcome
    )
    .before_loss
    .is_some());
    // The user states the first wasn't paid before the loss: it is paid as any other.
    wallet
        .confirm_not_paid_before_loss(
            &first_doubt.obligation,
            &first_doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert_eq!(status(&wallet, &first).await, RequestStatus::Open);
    assert_eq!(status(&wallet, &second).await, RequestStatus::MayBePaid);
    let paid = wallet
        .pay_request(&first, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert!(doubt_of(
        &wallet
            .pay_request(&second, PayOptions::default())
            .await
            .outcome
    )
    .before_loss
    .is_some());
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_settled_claim_at_the_new_service_ends_the_doubt() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut laptop = w.wallet("laptop").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    laptop.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    follow_loss(&mut laptop).await;
    assert_eq!(status(&laptop, &request).await, RequestStatus::MayBePaid);
    // The wallet pays it over the doubt: its claim at the new service is SETTLED.
    let doubt = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    // The laptop's doubt ends with it: the request reads paid, never "may be paid".
    laptop.receive(None).await.unwrap();
    assert_eq!(status(&laptop, &request).await, RequestStatus::Paid);
    let again = laptop
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    assert!(
        matches!(
            again.as_ref().err().and_then(PaykitError::refusal),
            Some(Refusal::AlreadyPaid)
        ),
        "{again:?}"
    );
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_request_created_inside_the_residual_window_is_paid_only_by_hand() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // An earlier request of bob's, which the loss's steps carry to the new service.
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    let uncertain_until_ms = follow_loss(&mut wallet).await;
    let loss = wallet.app().recovery_info().await.unwrap().unwrap().loss;
    // Bob's request is signed after the declaration plus the clock tolerance, but before the
    // end of the residual window plus the tolerance, and reaches the wallet only after the
    // window: the old service could have taken another own app's attempt on it.
    let created = uncertain_until_ms + CLOCK_TOLERANCE_MS - 1_000;
    assert!(created >= loss.declared_at_ms + CLOCK_TOLERANCE_MS);
    w.clock.advance_ms(created.saturating_sub(w.clock.now_ms()));
    let request = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    // Never paid automatically, a confirmation of its doubt passing nothing.
    let refused = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome;
    let doubt = doubt_of(&refused);
    assert!(doubt.before_loss.is_some(), "{doubt:?}");
    let refused = wallet
        .pay_request_automatically(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .outcome;
    assert!(doubt_of(&refused).before_loss.is_some());
    // By hand, it is met by the prompt that it may already be paid, and goes only on the
    // user's confirmation.
    let doubt = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    assert!(doubt.before_loss.is_some(), "{doubt:?}");
    assert_eq!(executions(&w), 0);
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_resume_never_repeats_a_dispatch_from_before_the_loss() {
    let w = World::new();
    w.rail.set_declaration(
        RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS).with_idempotent(true),
    );
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // Dispatched at the old service, its answer never coming: a resume on this idempotent
    // method without a history would repeat it.
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("in flight");
    };
    let obligation = wallet.payments().await.unwrap().remove(0).obligation;
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    // The new service's claim shows this app's attempt from before the loss open: the one
    // a resume would repeat.
    let read = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    let attempt = read.view.attempt(&attempt_id.to_bytes()).unwrap();
    assert_eq!(attempt.owner, wallet.app_id());
    assert_eq!(attempt.outcome, pubky_switch_core::claims::Outcome::Open);
    // The user states it wasn't paid before the loss: the obligation's doubt ends, and
    // still the dispatch this app made before the loss isn't repeated.
    wallet
        .confirm_not_paid_before_loss(&obligation, &kept_loss(&wallet).await, UserConfirmed::new())
        .await
        .unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        resumed.iter().any(|outcome| matches!(
            outcome,
            PayOutcome::Uncertain { attempt_id: id, .. } if *id == attempt_id
        )),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_provider_loss_newer_loss_puts_a_stated_obligation_in_doubt_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let doubt = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    wallet
        .confirm_not_paid_before_loss(
            &doubt.obligation,
            &doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert_eq!(status(&wallet, &request).await, RequestStatus::Open);
    // The new service is lost too: what the user stated was about the first loss.
    w.clock.advance_ms(60_000);
    declare_loss_of(&w, SWITCH_ORIGIN, LATER_SECRET);
    follow_loss(&mut wallet).await;
    let loss = wallet.app().recovery_info().await.unwrap().unwrap().loss;
    let kept: serde_json::Value = serde_json::from_slice(
        &wallet
            .app()
            .protocol_get("paykit", b"pl")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(kept["d"]["generation"], loss.generation);
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    let again = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    assert!(doubt_of(&again).before_loss.is_some(), "{again:?}");
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_confirm_not_paid_before_loss_refused_without_a_loss_kept() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // No loss followed: nothing to state, and nothing is written.
    let refused = wallet
        .confirm_not_paid_before_loss(&[7; 32], &some_loss(), UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"pn/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(status(&wallet, &request).await, RequestStatus::Open);
}

#[tokio::test]
async fn test_provider_loss_restore_refuses_a_loss_above_the_clients_generation() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    let id = wallet.app_id();
    drop(wallet);
    let backup: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let followed = backup["meta"]["generation"].as_u64().unwrap();
    // The kept loss as no run writes it: recovered at a generation after the client's.
    let mut raised = false;
    let state = layer_of(&backup)
        .into_iter()
        .map(|(key, mut value)| {
            if key == b"pl" {
                assert_eq!(value["d"]["generation"], followed);
                value["d"]["generation"] = serde_json::json!(followed + 1);
                raised = true;
            }
            (key, value)
        })
        .collect();
    assert!(raised);
    let backup = with_layer(&backup, state);
    let at = store();
    let refused = refused_restore(&w, backup, Arc::clone(&at)).await;
    assert!(is_refused(&refused), "{refused:?}");
    nothing_restored(&w, at, id).await;
}

/// The obligations a reconcile of `app` lists as in doubt after a provider loss.
async fn loss_doubts(app: &mut Paykit) -> Vec<Doubtful> {
    let report = app.reconcile().await.unwrap();
    report
        .doubtful
        .into_iter()
        .filter(|doubtful| doubtful.before_loss.is_some())
        .collect()
}

/// A wallet holding two of bob's requests, received before alice's service is lost and
/// followed there. Returns the world, the wallet and the requests.
async fn followed_with_requests() -> (World, Paykit, [RequestRef; 2]) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    (w, wallet, [first, second])
}

#[tokio::test]
async fn test_provider_loss_reconcile_lists_each_obligation_from_before_the_loss() {
    // Neither request was attempted at the new service: the reconcile lists both as in
    // doubt after the loss, with their payee and request and no attempt, the obligation the
    // refusal of a payment by hand names.
    let (w, mut wallet, requests) = followed_with_requests().await;
    let listed = loss_doubts(&mut wallet).await;
    assert_eq!(listed.len(), 2, "{listed:?}");
    for request in &requests {
        let doubtful = listed
            .iter()
            .find(|d| d.request_hash == Some(request.request_hash))
            .unwrap();
        assert_eq!(doubtful.payee, Some(w.bob_id()));
        assert!(doubtful.attempts.is_empty(), "{doubtful:?}");
        let refused = wallet
            .pay_request(request, PayOptions::default())
            .await
            .outcome;
        assert_eq!(doubt_of(&refused).obligation, doubtful.obligation);
    }
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_reconcile_lists_no_obligation_the_user_stated_not_paid() {
    let (_w, mut wallet, [first, second]) = followed_with_requests().await;
    let doubt = doubt_of(
        &wallet
            .pay_request(&first, PayOptions::default())
            .await
            .outcome,
    );
    wallet
        .confirm_not_paid_before_loss(
            &doubt.obligation,
            &doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    let listed = loss_doubts(&mut wallet).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].request_hash, Some(second.request_hash));
}

#[tokio::test]
async fn test_provider_loss_reconcile_lists_no_obligation_settled_at_the_new_service() {
    let (w, mut wallet, [first, second]) = followed_with_requests().await;
    let doubt = doubt_of(
        &wallet
            .pay_request(&first, PayOptions::default())
            .await
            .outcome,
    );
    let paid = wallet
        .pay_request(
            &first,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let listed = loss_doubts(&mut wallet).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].request_hash, Some(second.request_hash));
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_reconcile_lists_a_stated_obligation_again_after_a_newer_loss() {
    let (w, mut wallet, [first, _]) = followed_with_requests().await;
    let doubt = doubt_of(
        &wallet
            .pay_request(&first, PayOptions::default())
            .await
            .outcome,
    );
    wallet
        .confirm_not_paid_before_loss(
            &doubt.obligation,
            &doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert_eq!(loss_doubts(&mut wallet).await.len(), 1);
    w.clock.advance_ms(60_000);
    declare_loss_of(&w, SWITCH_ORIGIN, LATER_SECRET);
    follow_loss(&mut wallet).await;
    let listed = loss_doubts(&mut wallet).await;
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(listed
        .iter()
        .any(|d| d.request_hash == Some(first.request_hash) && d.obligation == doubt.obligation));
}

/// A request from before a loss, its doubt shown to the user under that loss, then a newer
/// loss followed: returns the doubt shown.
async fn shown_then_newer_loss(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
    step_run: bool,
) -> (RequestRef, PaymentDoubt) {
    w.clock.advance_ms(60_000);
    declare_loss(w);
    follow_loss(wallet).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    // Still inside the first loss's residual window: the request is doubtful, and its
    // record is written at this service before the next loss.
    let request = request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let shown = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    w.clock.advance_ms(60_000);
    let next = declare_loss_of(w, SWITCH_ORIGIN, LATER_SECRET);
    if step_run {
        follow_loss(wallet).await;
    } else {
        next.fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
        wallet.app_mut().refresh_identity().await.unwrap();
        let mut following = None;
        for _ in 0..4 {
            let info = wallet.app().recovery_info().await.unwrap().unwrap();
            if info.loss.generation > shown_generation(&shown) && !info.steps_done {
                following = Some(info);
                break;
            }
            let _ = wallet.receive(None).await;
        }
        assert!(
            following.is_some(),
            "the newer loss isn't followed before its step"
        );
    }
    (request, shown)
}

#[tokio::test]
async fn test_provider_loss_confirmed_doubt_of_an_older_loss_pays_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, shown) = shown_then_newer_loss(&w, &mut wallet, &mut shop, true).await;
    // The user confirms the doubt shown under the first loss: the newer loss decides now,
    // so the payment is refused with its doubt, and nothing is paid.
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(shown.clone()),
        )
        .await
        .outcome;
    let now = doubt_of(&paid);
    assert!(shown_generation(&now) > shown_generation(&shown), "{now:?}");
    assert_eq!(executions(&w), 0);
    // The doubt shown under the newer loss, confirmed, pays.
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(now),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_statement_about_an_older_loss_refused_after_a_newer_one() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, shown) = shown_then_newer_loss(&w, &mut wallet, &mut shop, true).await;
    // The user's answer to the prompt shown under the first loss comes late: it is refused,
    // naming the newer loss, and the obligation stays in doubt after it.
    let decisive = kept_loss(&wallet).await;
    let refused = wallet
        .confirm_not_paid_before_loss(
            &shown.obligation,
            &shown.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        refused.refusal(),
        Some(&Refusal::LossChanged {
            decisive: Some(decisive)
        })
    );
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"pn/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    assert_eq!(executions(&w), 0);
    // The answer to the doubt shown under the newer loss ends it.
    let again = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    assert_eq!(again.before_loss, Some(decisive));
    wallet
        .confirm_not_paid_before_loss(&again.obligation, &decisive, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(status(&wallet, &request).await, RequestStatus::Open);
}

#[tokio::test]
async fn test_provider_loss_statement_about_an_older_loss_refused_before_the_newer_ones_step() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, shown) = shown_then_newer_loss(&w, &mut wallet, &mut shop, false).await;
    // The newer loss is followed, its step not run yet: an answer about the first loss is
    // refused, not taken for a statement the newer loss's step drops.
    let refused = wallet
        .confirm_not_paid_before_loss(
            &shown.obligation,
            &shown.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(Refusal::LossChanged { decisive: Some(loss) })
                if loss.generation() > shown_generation(&shown)
        ),
        "{refused:?}"
    );
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"pn/")
        .await
        .unwrap()
        .is_empty());
    follow_loss(&mut wallet).await;
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    assert_eq!(executions(&w), 0);
}

/// A host store that stops exactly the loss callback's local commit.
#[derive(Default)]
struct LossCommitStore {
    inner: pubky_switch_client::store::MemoryStore,
    mode: std::sync::atomic::AtomicU8,
    reached: std::sync::atomic::AtomicBool,
}

impl pubky_switch_client::store::StateStore for LossCommitStore {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_switch_client::store::KeyValue>, pubky_switch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_switch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
    ) -> Result<pubky_switch_client::store::AcquireOutcome, pubky_switch_client::error::StoreError>
    {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}
impl LossCommitStore {
    fn commit_with(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
        apply: impl FnOnce(
            pubky_switch_client::store::WriteBatch,
        ) -> Result<
            pubky_switch_client::store::CommitOutcome,
            pubky_switch_client::error::StoreError,
        >,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        use pubky_switch_client::error::StoreError;
        use pubky_switch_client::store::Write;
        use std::sync::atomic::Ordering::SeqCst;
        let loss = batch
            .writes()
            .iter()
            .any(|write| matches!(write, Write::Put { key, .. } if key == b"p/paykit/pl"));
        if loss && self.mode.load(SeqCst) != 0 {
            // The boundary and statements change in the same host transaction.
            assert!(batch.writes().iter().any(
                |write| matches!(write, Write::DeletePrefix { prefix, .. } if prefix == b"p/paykit/pn/")
            ));
            assert!(batch.writes().iter().any(
                |write| matches!(write, Write::Put { key, .. } if key.starts_with(b"p/paykit/missed/"))
            ));
            self.reached.store(true, SeqCst);
            match self.mode.swap(0, SeqCst) {
                1 => return Err(StoreError::Unavailable),
                2 => {
                    let applied = apply(batch)?;
                    if applied != pubky_switch_client::store::CommitOutcome::Applied {
                        return Ok(applied);
                    }
                    return Err(StoreError::Unknown);
                }
                _ => unreachable!(),
            }
        }
        apply(batch)
    }
}

async fn loss_commit_reload(applied: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    let w = World::new();
    let storage = Arc::new(LossCommitStore::default());
    let mut wallet = w.wallet_on("wallet", storage.clone()).await;
    let mut shop = w.shop().await;
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let old = kept_loss(&wallet).await;
    let doubt = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    wallet
        .confirm_not_paid_before_loss(&doubt.obligation, &old, UserConfirmed::new())
        .await
        .unwrap();
    let statement = wallet.app().protocol_scan("paykit", b"pn/").await.unwrap();
    assert_eq!(statement.len(), 1);
    let id = wallet.app_id();
    w.clock.advance_ms(60_000);
    declare_loss_of(&w, SWITCH_ORIGIN, LATER_SECRET);
    storage.mode.store(if applied { 2 } else { 1 }, SeqCst);
    let _ = wallet.app_mut().refresh_identity().await;
    for _ in 0..4 {
        if storage.reached.load(SeqCst) {
            break;
        }
        let _ = wallet.receive(None).await;
    }
    assert!(
        storage.reached.load(SeqCst),
        "the loss commit fault must be reached"
    );
    drop(wallet);
    let mut wallet = Paykit::load(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let retained = kept_loss(&wallet).await;
    let statements = wallet.app().protocol_scan("paykit", b"pn/").await.unwrap();
    if applied {
        assert!(retained.generation() > old.generation());
        assert!(statements.is_empty());
    } else {
        assert_eq!(retained, old);
        assert_eq!(statements, statement);
    }
    // The followed newer loss still decides before retrying its callback.
    wallet
        .confirm_not_paid_before_loss(&doubt.obligation, &old, UserConfirmed::new())
        .await
        .expect_err("an older statement must not be recorded after reload");
    follow_loss(&mut wallet).await;
    assert!(kept_loss(&wallet).await.generation() > old.generation());
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"pn/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(status(&wallet, &request).await, RequestStatus::MayBePaid);
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_failed_commit_retains_boundary_and_statements_for_retry() {
    loss_commit_reload(false).await;
}

#[tokio::test]
async fn test_provider_loss_applied_unknown_commit_reloads_boundary_and_cleared_statements() {
    loss_commit_reload(true).await;
}

async fn loss_with_statement() -> (World, Paykit, RequestRef, Vec<(Vec<u8>, Vec<u8>)>) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let doubt = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    wallet
        .confirm_not_paid_before_loss(
            &doubt.obligation,
            &doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    let statement = wallet.app().protocol_scan("paykit", b"pn/").await.unwrap();
    assert_eq!(statement.len(), 1);
    (w, wallet, request, statement)
}

#[tokio::test]
async fn test_provider_loss_current_statement_and_boundary_survive_restore() {
    let (w, mut wallet, request, statement) = loss_with_statement().await;
    let boundary = kept_loss(&wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = restored(&w, &backup).await;
    assert_eq!(kept_loss(&restored).await, boundary);
    assert_eq!(
        restored
            .app()
            .protocol_scan("paykit", b"pn/")
            .await
            .unwrap(),
        statement
    );
    restored.receive(None).await.unwrap();
    restored.reconcile().await.unwrap();
    restored.end_reconcile(UserConfirmed::new()).await.unwrap();
    let options = crate::common::over_hold(&restored, &request).await;
    let paid = restored
        .pay_request(&request, options)
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }));
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_unknown_current_boundary_refuses_read_and_backup() {
    let (w, mut wallet, request, _) = loss_with_statement().await;
    let mut value: serde_json::Value = serde_json::from_slice(
        &wallet
            .app()
            .protocol_get("paykit", b"pl")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    value["d"]["extra"] = serde_json::json!(true);
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: b"pl".to_vec(),
                value: serde_json::to_vec(&value).unwrap(),
            }],
        )
        .await
        .unwrap();
    wallet
        .request(&request)
        .await
        .expect_err("the current typed boundary must refuse unknown members");
    wallet
        .backup()
        .await
        .expect_err("a retained corrupt boundary cannot be backed up");
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_unknown_boundary_in_saved_backup_is_refused() {
    let (w, mut wallet, _, _) = loss_with_statement().await;
    let id = wallet.app_id();
    let backup: serde_json::Value =
        serde_json::from_slice(&wallet.backup().await.unwrap().to_bytes().unwrap()).unwrap();
    let mut layer = layer_of(&backup);
    let value = &mut layer.iter_mut().find(|(key, _)| key == b"pl").unwrap().1;
    value["d"]["extra"] = serde_json::json!(true);
    let at = store();
    let error = refused_restore(&w, with_layer(&backup, layer), at.clone()).await;
    assert!(is_refused(&error), "{error:?}");
    nothing_restored(&w, at, id).await;
}

#[tokio::test]
async fn test_provider_loss_false_statement_in_saved_backup_is_refused() {
    let (w, mut wallet, _, _) = loss_with_statement().await;
    let id = wallet.app_id();
    let backup: serde_json::Value =
        serde_json::from_slice(&wallet.backup().await.unwrap().to_bytes().unwrap()).unwrap();
    let mut layer = layer_of(&backup);
    let value = &mut layer
        .iter_mut()
        .find(|(key, _)| key.starts_with(b"pn/"))
        .unwrap()
        .1;
    value["d"] = serde_json::json!(false);
    let at = store();
    let error = refused_restore(&w, with_layer(&backup, layer), at.clone()).await;
    assert!(is_refused(&error), "{error:?}");
    nothing_restored(&w, at, id).await;
}

async fn duplicate_loss_entry_is_refused(prefix: &[u8], future: bool) {
    let (w, mut wallet, _, _) = loss_with_statement().await;
    let id = wallet.app_id();
    let backup: serde_json::Value =
        serde_json::from_slice(&wallet.backup().await.unwrap().to_bytes().unwrap()).unwrap();
    let followed = backup["meta"]["generation"].as_u64().unwrap();
    let mut layer = layer_of(&backup);
    let mut duplicate = layer
        .iter()
        .find(|(key, _)| key.starts_with(prefix))
        .unwrap()
        .clone();
    if future {
        duplicate.1["d"]["generation"] = serde_json::json!(followed + 1);
    }
    layer.push(duplicate);
    let at = store();
    let error = refused_restore(&w, with_layer(&backup, layer), at.clone()).await;
    assert!(is_refused(&error), "{error:?}");
    nothing_restored(&w, at, id).await;
}

#[tokio::test]
async fn test_provider_loss_duplicate_boundary_in_backup_is_refused() {
    duplicate_loss_entry_is_refused(b"pl", true).await;
}

#[tokio::test]
async fn test_provider_loss_identical_boundaries_in_backup_are_refused() {
    duplicate_loss_entry_is_refused(b"pl", false).await;
}

#[tokio::test]
async fn test_provider_loss_duplicate_statements_in_backup_are_refused() {
    duplicate_loss_entry_is_refused(b"pn/", false).await;
}

async fn journaled_orphan_report(after_loss: bool) {
    let w = World::new();
    w.rail.set_declaration(RailDeclaration::new(
        paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
    ));
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("pending");
    };
    let obligation = wallet.payments().await.unwrap().remove(0).obligation;
    w.clock.advance_ms(2 * 3_600_000);
    if after_loss {
        declare_loss(&w);
        follow_loss(&mut wallet).await;
    }
    wallet.app_mut().read_claim(&obligation).await.unwrap();
    let client_doubt = wallet
        .app()
        .claim_doubt(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert!(
        client_doubt.orphaned.contains(&attempt_id.to_bytes()),
        "{client_doubt:?}"
    );
    let report = wallet.reconcile().await.unwrap();
    let doubtful = report
        .doubtful
        .iter()
        .find(|d| d.obligation == obligation)
        .expect("an orphaned attempt must be reported");
    assert_eq!(doubtful.before_loss.is_some(), after_loss);
    assert_eq!(doubtful.attempts, [attempt_id], "{report:?}");
    if after_loss {
        assert_eq!(doubtful.before_loss, Some(kept_loss(&wallet).await));
        assert_eq!(doubtful.payee, Some(request.payee));
        assert_eq!(doubtful.request_hash, Some(request.request_hash));
    }
    let again = wallet.reconcile().await.unwrap();
    assert_eq!(again.doubtful, report.doubtful);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_provider_loss_reconcile_retains_journaled_orphan_after_loss() {
    journaled_orphan_report(true).await;
}

#[tokio::test]
async fn test_provider_loss_reconcile_retains_journaled_orphan_without_loss() {
    journaled_orphan_report(false).await;
}
