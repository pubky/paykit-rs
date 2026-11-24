//! A provider loss (Paykit spec 7.3, "After a provider loss"; Pubky Switch spec 12.6): the
//! signer declares alice's generation lost at a new service, the old one stops answering,
//! and her apps follow the loss there.

mod notice;
mod shared_hold;

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
pub(super) fn declare_loss(w: &World) -> Arc<ScriptedSwitch> {
    declare_loss_of(w, ALICE_ORIGIN, NEXT_SECRET)
}

/// The signer replaces the first recovery service after a second declared loss.
pub(super) fn declare_second_loss(w: &World) -> Arc<ScriptedSwitch> {
    declare_loss_of(w, SWITCH_ORIGIN, LATER_SECRET)
}

/// The signer replaces the second recovery service with a new service key.
pub(super) fn declare_third_loss(w: &World) -> Arc<ScriptedSwitch> {
    declare_loss_of(w, SWITCH_ORIGIN, [123; 32])
}

/// [`declare_loss`], with the next `identity.json` still listing the lost generation's
/// service key, as a signer could: only the rule that nothing from before a loss carries
/// across then refuses the lost generation's evidence.
pub(super) fn declare_loss_keeping_key(w: &World) -> Arc<ScriptedSwitch> {
    let lost = w.homeserver.identity_doc(&w.alice_id()).unwrap();
    let entry = pubky_switch_core::records::ServiceKeyEntry {
        generation: lost.generation(),
        key: lost.inbox().service_key,
    };
    declare_loss_with(w, ALICE_ORIGIN, NEXT_SECRET, Some(entry))
}

/// [`declare_loss`] of the service at `lost_origin`, the new one under `secret`.
fn declare_loss_of(w: &World, lost_origin: &str, secret: [u8; 32]) -> Arc<ScriptedSwitch> {
    declare_loss_with(w, lost_origin, secret, None)
}

fn declare_loss_with(
    w: &World,
    lost_origin: &str,
    secret: [u8; 32],
    kept_key: Option<pubky_switch_core::records::ServiceKeyEntry>,
) -> Arc<ScriptedSwitch> {
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
            body.service_keys.extend(kept_key);
        })
        .unwrap();
    next.deliver_identity(&doc).unwrap();
    w.router.remove(lost_origin);
    w.router.add_switch(SWITCH_ORIGIN, &next);
    next
}

/// Follow the new identity and finish the native recovery reads.
pub(super) async fn follow_generation(app: &mut Paykit) -> u64 {
    app.app_mut().refresh_identity().await.unwrap();
    for _ in 0..4 {
        app.receive(None).await.unwrap();
    }
    crate::common::finish_listing(app).await;
    let info = app.app().recovery_info().await.unwrap().unwrap();
    assert!(info.steps_done, "{info:?}");
    assert!(info.loss.uncertain_until_ms > info.loss.declared_at_ms);
    info.loss.uncertain_until_ms
}

/// Recover at the new service and complete payment reconciliation.
pub(super) async fn follow_loss(app: &mut Paykit) -> u64 {
    let until = follow_generation(app).await;
    // Reconciliation requests a fresh listing, even when receive finished its own listing.
    let pages = crate::certificate::client_listing_requests(app.app().cert());
    let mut reconciled = app.reconcile().await;
    for _ in 1..pages.div_ceil(pubky_switch_client::app::LISTING_PAGES_PER_READ as usize) {
        let Err(error) = &reconciled else { break };
        assert!(
            matches!(
                error.refusal(),
                Some(paykit_sdk::Refusal::NotNow(
                    paykit_sdk::NotNowReason::RecordsUnlisted { .. }
                        | paykit_sdk::NotNowReason::RecordsNotListed
                ))
            ),
            "{error}"
        );
        reconciled = app.reconcile().await;
    }
    reconciled.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    until
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

/// The period's record on the same side as the request's record.
fn period_target(
    request: &RequestRef,
    family: paykit_lib::collections::Collection,
    period: &paykit_lib::periods::PeriodRef,
) -> pubky_switch_client::app::RecordTarget {
    pubky_switch_client::app::RecordTarget::new(
        activity_target(request, family).collection,
        &paykit_lib::derive::natural_key::period_record(
            &request.payee,
            &request.request_hash,
            period.start_secs(),
        ),
    )
}

async fn confirm_reported_pauses(app: &mut Paykit) {
    for shown in app.divergent_records().await.unwrap() {
        app.confirm_divergent(&shown, UserConfirmed::new())
            .await
            .unwrap();
    }
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
    recover_activity_writes(w, wallet, phone, &[(target, delete_held)]).await
}

async fn recover_activity_writes(
    w: &World,
    wallet: &mut Paykit,
    phone: &mut Paykit,
    targets: &[(&pubky_switch_client::app::RecordTarget, bool)],
) -> Arc<ScriptedSwitch> {
    let writes = competing_activity_writes(w, wallet, phone, targets).await;
    recover_competing_writes(w, wallet, phone, targets, writes).await
}

struct CompetingWrites {
    records: Vec<pubky_switch_client::client::answers::UploadedOp>,
    applied: Vec<([u8; 32], pubky_switch_core::ops::RecordVersion)>,
}

async fn competing_activity_writes(
    w: &World,
    wallet: &mut Paykit,
    phone: &mut Paykit,
    targets: &[(&pubky_switch_client::app::RecordTarget, bool)],
) -> CompetingWrites {
    use pubky_switch_client::app::OpBatch;
    use pubky_switch_client::client::answers::UploadedOp;
    use pubky_switch_client::testing::Fault;
    use pubky_switch_core::ops::{Entry, OpKind, OpsRequest, SignedOp};
    let mut batch = OpBatch::for_protocol("paykit");
    let mut applied = Vec::new();
    for &(target, delete) in targets {
        let key = wallet.app().record_key(target).unwrap();
        let current = wallet.app_mut().read_record(target).await.unwrap().unwrap();
        let value = current.content.value().unwrap();
        let base = wallet
            .app_mut()
            .put_record(target, value, vec![])
            .await
            .unwrap();
        phone.app_mut().read_record(target).await.unwrap().unwrap();
        let mut alternate: serde_json::Value = serde_json::from_slice(value).unwrap();
        if target.collection.name == paykit_lib::collections::Collection::PayerActivity.name() {
            alternate["manual_only"] = true.into();
        }
        let alternate = serde_json::to_vec(&alternate).unwrap();
        w.competing.lock().unwrap().insert(key, alternate.clone());
        let op = phone
            .app_mut()
            .record_op(target, Some(&alternate), vec![])
            .await
            .unwrap();
        batch = batch.entry(op);
        let write = if delete {
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
        assert_eq!(write.version.version, base.version.version + 1);
        applied.push((key, write.version));
    }
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    assert!(phone.app_mut().submit_batch(batch).await.is_err());
    let missed = w.alice.switch().unreached_bodies().pop().unwrap();
    let records = OpsRequest::parse(&missed)
        .unwrap()
        .entries
        .into_iter()
        .filter_map(|entry| {
            let Entry::Signed(op) = entry else {
                return None;
            };
            let verified =
                SignedOp::verify_with_cert(&op, phone.app().cert(), &w.alice_id()).unwrap();
            match &verified.body().kind {
                OpKind::RecordPut(write) if applied.iter().any(|(key, _)| *key == write.key) => {
                    Some(UploadedOp {
                        op,
                        cert: phone.app().cert().signed().clone(),
                        proof: None,
                    })
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(records.len(), targets.len());
    CompetingWrites { records, applied }
}

async fn recover_competing_writes(
    w: &World,
    wallet: &mut Paykit,
    phone: &mut Paykit,
    targets: &[(&pubky_switch_client::app::RecordTarget, bool)],
    writes: CompetingWrites,
) -> Arc<ScriptedSwitch> {
    use pubky_switch_client::client::answers::RecoveryUpload;
    use pubky_switch_client::client::{Credentials, SwitchClient};
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
                records: writes.records,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
    wallet.app_mut().refresh_identity().await.unwrap();
    wallet.app_mut().follow_provider().await.unwrap();
    for ((target, _), (key, version)) in targets.iter().zip(writes.applied) {
        let held = wallet.app_mut().read_record(target).await.unwrap().unwrap();
        assert_eq!(held.version, version);
        // An honest lost service: the wallet's proved write stays current, and the other
        // app's write on the same base is divergent (Pubky Switch wire 7.4.14).
        assert!(held.divergent && !held.different_write_at_held_version);
        assert_eq!(next.record(&key).unwrap().state.version, held.version);
    }
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
    assert!(held.divergent && !held.different_write_at_held_version);
    assert_eq!(next.record(&key).unwrap().state.version, held.version);
    follow_loss(&mut wallet).await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    // The record is paused for the user's confirmation of what it shows (Paykit spec 6).
    assert!(
        matches!(refused.refusal(), Some(Refusal::Divergent { records }) if records.iter().any(|r| r.record == key && r.paused && r.divergent)),
        "{refused:?}"
    );
    assert_eq!(executions(&w), 0);
    // The floor's check ran over the paused record's current write, which reads: a
    // `manual_only` mark it owed would be a stop, written over a divergent record all the
    // same (Paykit spec 6). The record stays paused.
    let _ = owed_floor;
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"fo/")
        .await
        .unwrap()
        .is_empty());
    assert!(wallet
        .divergent_records()
        .await
        .unwrap()
        .iter()
        .any(|r| r.record == key && r.paused));
    // Confirming the record the refusal shows ends its pause (Paykit spec 6). The request
    // was payable before the loss and isn't SETTLED at the new service, so a payment by hand
    // still waits on that doubt (Paykit spec 7.3): nothing is paid.
    let (doubt, passed) = pay_until_the_loss_doubt(&mut wallet, &request).await;
    assert_eq!(passed, [Passed::Divergent]);
    in_doubt_of_the_loss(&mut wallet, &request, &doubt).await;
    assert!(wallet
        .divergent_records()
        .await
        .unwrap()
        .iter()
        .all(|r| r.record != key || !r.paused));
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_receipt_waits_for_an_accepted_activity_successor() {
    disputed_receipt(false, false).await;
}

#[tokio::test]
async fn test_provider_loss_disputed_receipt_expires_with_its_notice() {
    disputed_receipt(true, false).await;
}

#[tokio::test]
async fn test_provider_loss_receipt_waits_over_a_disputed_signed_delete() {
    disputed_receipt(false, true).await;
}

async fn disputed_receipt(expires: bool, delete: bool) {
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
    let next =
        recover_activity_with_phone_write(&w, &mut wallet, &mut phone, &target, delete).await;
    let key = wallet.app().record_key(&target).unwrap();
    follow_loss(&mut wallet).await;
    // A divergent value waits for the user's confirmation, which this user doesn't give. A
    // divergent delete shows nothing to confirm: confirmed or not, it holds the receipt as
    // another write at its version does (Paykit spec 6).
    if delete {
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
    }
    assert!(
        wallet
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .unwrap()
            .divergent
    );
    w.clock.advance_ms(11 * 60_000);
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let before = next.record(&key).unwrap().revision;
    let receipt = shop.confirm_payment(&sale).await.unwrap().unwrap();
    let delivery = shop.app_mut().flush_message(&receipt).await;
    let read = wallet.receive(None).await.unwrap();
    let kept = kept_over(&wallet, &key).await;
    assert_eq!(
        kept.len(),
        1,
        "keep the original receipt: {delivery:?} {read:?}"
    );
    assert!(kept[0].0.ends_with(receipt.as_str()));
    assert_eq!(next.record(&key).unwrap().revision, before);
    let id = wallet.app_id();
    drop(wallet);
    let mut wallet = w.load_on_clock(&w.alice, disk, id, w.clock.clone()).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(kept_over(&wallet, &key).await, kept);
    let other = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.request(&other).await.unwrap().request, other);
    assert_eq!(kept_over(&wallet, &key).await, kept);

    if expires {
        w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - 1);
        wallet.receive(None).await.unwrap();
        assert_eq!(kept_over(&wallet, &key).await, kept);
        w.clock.advance_ms(1);
        wallet.receive(None).await.unwrap();
        assert!(kept_over(&wallet, &key).await.is_empty());
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
    // A successor ends the divergence; the pause ends with the user's confirmation.
    accept_activity_successor(&w, &mut wallet, &mut phone, &target).await;
    confirm_reported_pauses(&mut wallet).await;
    wallet.receive(None).await.unwrap();
    assert!(kept_over(&wallet, &key).await.is_empty());
    assert!(wallet.request(&request).await.unwrap().receipt.is_some());
    assert_eq!(executions(&w), 1);
}

/// The messages `app` keeps for `record`: over its pause (`dw/`) or as replies before a
/// record it can't read (`pr/`), each by its key.
async fn kept_over(app: &Paykit, record: &[u8; 32]) -> Vec<(String, Vec<u8>)> {
    use pubky_switch_core::enc::b64;
    let mut kept = Vec::new();
    for prefix in [format!("dw/{}/", b64(record)), "pr/".to_string()] {
        for (key, value) in app
            .app()
            .protocol_scan("paykit", prefix.as_bytes())
            .await
            .unwrap()
        {
            let key = String::from_utf8(key).unwrap();
            if prefix.starts_with("dw/") || key.contains(&b64(record)) {
                kept.push((key, value));
            }
        }
    }
    kept
}

/// The phone writes its divergent write again, as the record's successor: the activity it
/// wrote, with its restriction, becomes the current write, and the record is no longer
/// divergent.
async fn accept_activity_successor(
    w: &World,
    wallet: &mut Paykit,
    phone: &mut Paykit,
    target: &pubky_switch_client::app::RecordTarget,
) {
    let current = phone.app_mut().read_record(target).await.unwrap().unwrap();
    assert!(current.divergent && !current.different_write_at_held_version);
    let content = w.competing.lock().unwrap()[&current.key].clone();
    if target.collection.name == paykit_lib::collections::Collection::PayerActivity.name() {
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&content).unwrap()["manual_only"],
            true
        );
    }
    phone
        .app_mut()
        .put_record(target, &content, vec![])
        .await
        .unwrap();
    let successor = wallet.app_mut().read_record(target).await.unwrap().unwrap();
    assert!(!successor.divergent && !successor.different_write_at_held_version);
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
    let before = next.record(&key).unwrap().revision;
    w.rail.settle(&attempt_id, true);
    w.rail.set_declaration(
        RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true)
        .with_history(true),
    );
    // The record is divergent: the completed payment's write waits for the user or a
    // successor, across a reload, and nothing is paid again (Paykit spec 6).
    let held = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(held.as_slice(), [PayOutcome::RecordDivergent { .. }]),
        "{held:?}"
    );
    assert_eq!(next.record(&key).unwrap().revision, before);
    let id = wallet.app_id();
    drop(wallet);
    let mut wallet = w.load_on_clock(&w.alice, disk, id, w.clock.clone()).await;
    let held = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(held.as_slice(), [PayOutcome::RecordDivergent { .. }]),
        "{held:?}"
    );
    assert_eq!(next.record(&key).unwrap().revision, before);
    assert_eq!(w.rail.executions(&attempt_id), 1);
    // A successor ends the divergence; the pause ends with the user's confirmation.
    accept_activity_successor(&w, &mut wallet, &mut phone, &target).await;
    confirm_reported_pauses(&mut wallet).await;
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
    // The restored app reads the service's current write, the wallet's own proved one, and
    // the other app's write beside it divergent: the record is paused for the user, and
    // nothing is paid (Paykit spec 6).
    let first = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert!(first.divergent && !first.different_write_at_held_version);
    assert!(
        serde_json::from_slice::<serde_json::Value>(first.content.value().unwrap()).unwrap()
            ["manual_only"]
            .is_null()
    );
    assert!(wallet
        .divergent_records()
        .await
        .unwrap()
        .iter()
        .any(|r| r.record == key && r.paused));
    assert_eq!(executions(&w), 0);
    // The user answers the restore's question, confirms the restore's hold and what the
    // paused record shows (Switch spec 12.4; Paykit spec 6). A payment by hand then still
    // waits on the loss's doubt (Paykit spec 7.3): nothing is paid.
    let (doubt, passed) = pay_until_the_loss_doubt(&mut wallet, &request).await;
    assert_eq!(
        passed,
        [
            Passed::RestoreQuestion,
            Passed::Divergent,
            Passed::RestoreHold
        ]
    );
    in_doubt_of_the_loss(&mut wallet, &request, &doubt).await;
    assert!(wallet
        .divergent_records()
        .await
        .unwrap()
        .iter()
        .all(|r| r.record != key || !r.paused));
    assert_eq!(executions(&w), 0);
}

#[tokio::test]
async fn test_provider_loss_disputed_delete_protects_its_cancellation_from_expiry_and_eviction() {
    let w = World::new();
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new()
                .pay()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            lists.clone(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
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
    let wallet_id = wallet.app_id();
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
        next.send_plaintext(w.bob_id(), shop.app().cert(), &signed, &[wallet_id])
            .unwrap();
    };
    lists.flag(&w.bob_id());
    cancel(request.request_hash);
    wallet.receive(None).await.unwrap();
    let stop = format!(
        "kc/{}/{}/{:020}",
        pubky_switch_core::enc::b64(&record),
        pubky_switch_core::enc::b64(&shop.app().cert().hash()),
        wallet.app().own_identity().generation(),
    )
    .into_bytes();
    assert!(
        wallet
            .app()
            .protocol_get("paykit", &stop)
            .await
            .unwrap()
            .is_some(),
        "keep the known request's cancellation on admission"
    );
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 0);
    lists.unflag(&w.bob_id());
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
    accept_activity_successor(&w, &mut wallet, &mut phone, &target).await;
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
    // The record is divergent: the proof from the signed request's recipient is kept over it
    // for the user's confirmation, not dropped as the flagged sender's spam (Paykit spec 6).
    let key = shop.app().record_key(&target).unwrap();
    let kept = kept_over(&shop, &key).await;
    assert!(
        kept.iter().any(|(id, _)| id.ends_with(proof.as_str())),
        "keep the original proof from the signed request's recipient: {kept:?}"
    );
    assert_eq!(shop.spam_count(&w.bob_id()).await.unwrap(), 0);
    // The credit waits; a cancellation, a stop, would go over the divergent record.
    for refusal in [
        shop.verify_payment(&key).await.unwrap_err(),
        shop.confirm_payment(&key).await.unwrap_err(),
    ] {
        // Nothing is credited: the record holds no proof while the proof is kept, or the
        // record is paused for the user.
        assert!(
            matches!(
                refusal.refusal(),
                Some(Refusal::NotFound) | Some(Refusal::Divergent { .. })
            ),
            "{refusal:?}"
        );
    }
    accept_activity_successor(&w, &mut shop, &mut phone, &target).await;
    confirm_reported_pauses(&mut shop).await;
    shop.receive(None).await.unwrap();
    assert!(kept_over(&shop, &key).await.is_empty());
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
    // The user's way forward for the credit: the proof predates the loss, so a plain credit
    // asks the user first. With the user's consent, the credit is still refused once: its
    // refusal shows the record for a credit, which the confirmations from the list didn't
    // cover. Confirming exactly that lets it go, once (Paykit spec 6, 8.1 step 4). Nothing
    // is receipted before.
    assert!(matches!(
        shop.confirm_payment(&key).await.unwrap_err().refusal(),
        Some(Refusal::MaybeCredited)
    ));
    assert_eq!(
        sale_status(&shop, &key).await,
        paykit_sdk::PayeeStatus::ProofReceived
    );
    let (receipt, refused_divergent) = credit_as_a_host(&mut shop, &key).await;
    assert_eq!(refused_divergent, 1);
    assert_eq!(
        sale_status(&shop, &key).await,
        paykit_sdk::PayeeStatus::Receipted
    );
    assert_eq!(executions(&w), 1);
    let again = shop
        .confirm_payment_with(
            &key,
            paykit_sdk::ReceiptDetails::default()
                .with_maybe_credited_confirmed(UserConfirmed::new()),
        )
        .await
        .unwrap();
    assert_eq!(again, Some(receipt));
}

/// Credit `key` as a host does after a provider loss: with the user's consent that the
/// payment may be credited already, confirming each divergent refusal as it shows its
/// records, and reading again until their kept writes are written. Returns the receipt and
/// how many divergent refusals, each shown for a credit, the credit went through.
async fn credit_as_a_host(app: &mut Paykit, key: &[u8; 32]) -> (String, usize) {
    let mut refused_divergent = 0;
    for _ in 0..4 {
        let consent = paykit_sdk::ReceiptDetails::default()
            .with_maybe_credited_confirmed(UserConfirmed::new());
        match app.confirm_payment_with(key, consent).await {
            Ok(Some(receipt)) => return (receipt, refused_divergent),
            Ok(None) => panic!("no receipt for the credit"),
            Err(refused) if matches!(refused.refusal(), Some(Refusal::Divergent { .. })) => {
                assert!(
                    crate::divergent::shown_for_a_credit(&refused),
                    "{refused:?}"
                );
                refused_divergent += 1;
                crate::divergent::confirm_refused(app, &refused).await;
                app.receive(None).await.unwrap();
            }
            Err(other) => panic!("{other:?}"),
        }
    }
    panic!("the credit never went");
}

/// `doubt` is the doubt of the loss `app` followed, of `request`'s obligation, which the
/// request reads as and `app` lists (Paykit spec 7.3).
async fn in_doubt_of_the_loss(
    app: &mut Paykit,
    request: &paykit_sdk::RequestRef,
    doubt: &PaymentDoubt,
) {
    assert_eq!(doubt.before_loss, Some(kept_loss(app).await));
    assert_eq!(status(app, request).await, RequestStatus::MayBePaid);
    assert!(loss_doubts(app)
        .await
        .iter()
        .any(|d| d.request_hash == Some(request.request_hash) && d.obligation == doubt.obligation));
}

/// What a payment by hand passed through on its way to the loss's doubt, in order.
#[derive(Debug, PartialEq, Eq)]
enum Passed {
    /// A divergent refusal, its records confirmed.
    Divergent,
    /// A restore's hold, handed back.
    RestoreHold,
    /// The restore's question, answered, and its reconciliation ended.
    RestoreQuestion,
}

/// Pay `request` by hand as a host does after a provider loss, through each refusal the
/// user is shown and decides on: it confirms what a divergent refusal shows and reads again,
/// hands back the hold a restore set, and answers the restore's question. It stops at the
/// loss's own doubt (Paykit spec 7.3): whatever the user confirmed before, the payment
/// doesn't go while the obligation may have been paid before the loss. Returns that doubt and
/// what the payment passed through, in order.
async fn pay_until_the_loss_doubt(
    app: &mut Paykit,
    request: &paykit_sdk::RequestRef,
) -> (PaymentDoubt, Vec<Passed>) {
    use pubky_switch_client::app::Refusal as ClientRefusal;
    let mut options = PayOptions::default();
    let mut passed = Vec::new();
    for _ in 0..8 {
        let refused = match app.pay_request(request, options.clone()).await.outcome {
            Ok(paid) => panic!("paid while the loss leaves it in doubt: {paid:?}"),
            Err(refused) => refused,
        };
        match refused.refusal() {
            Some(Refusal::Divergent { .. }) => {
                passed.push(Passed::Divergent);
                crate::divergent::confirm_refused(app, &refused).await;
                app.receive(None).await.unwrap();
            }
            Some(Refusal::HeldAfterRestore { held }) => {
                passed.push(Passed::RestoreHold);
                options = options.with_possibly_cancelled_confirmed((**held).clone());
            }
            Some(Refusal::MayBePaid {
                doubt: Some(doubt), ..
            }) if doubt.before_loss.is_some() => return ((**doubt).clone(), passed),
            _ if matches!(
                refused,
                PaykitError::Client(pubky_switch_client::app::AppError::Refused(
                    ClientRefusal::RestoreQuestionOpen
                ))
            ) =>
            {
                passed.push(Passed::RestoreQuestion);
                app.app_mut()
                    .confirm_restored_registrations(UserConfirmed::new())
                    .await
                    .unwrap();
                app.reconcile().await.unwrap();
                app.end_reconcile(UserConfirmed::new()).await.unwrap();
                app.receive(None).await.unwrap();
            }
            _ => panic!("{refused:?}"),
        }
    }
    panic!("the payment never reached the loss's doubt");
}

#[tokio::test]
async fn test_provider_loss_disputed_subscription_classifies_replies_by_its_signed_payer() {
    use crate::periods::{january_details, monthly, period_proof};
    use paykit_lib::collections::Collection;
    use paykit_sdk::AppScopes;
    use pubky_switch_client::app::AppEvent;

    for delete_parent in [false, true] {
        let w = World::new();
        let lists = Arc::new(crate::common::ListsStore::default());
        let scopes = AppScopes::new()
            .request()
            .manage_contacts()
            .build()
            .unwrap();
        let mut shop = w
            .join_with(
                &w.alice,
                "shop",
                scopes.clone(),
                PaykitConfig::default(),
                lists.clone(),
            )
            .await;
        let mut phone = w.join(&w.alice, "phone", scopes).await;
        let pay = AppScopes::new().pay().build().unwrap();
        let mut buyer = w.join(&w.bob, "buyer", pay.clone()).await;
        let guest = w.join(&w.carol, "guest", pay).await;
        buyer.app_mut().invite(&w.alice_id()).await.unwrap();
        let request = request_from(&mut shop, w.bob_id(), monthly(w.clock.now_ms(), None)).await;
        let target = activity_target(&request, Collection::PayeeActivity);
        let next =
            recover_activity_with_phone_write(&w, &mut shop, &mut phone, &target, delete_parent)
                .await;
        follow_loss(&mut shop).await;

        // Carol's certificate and replies are valid, but the signed request names Bob.
        for kind in [
            "paykit.payment_request_acceptance",
            "paykit.payment_request_rejection",
            "paykit.payment_request_cancellation",
        ] {
            let reply = w.sign_as(
                &w.carol,
                "guest",
                &w.alice,
                kind,
                vec![request.request_hash],
                Default::default(),
            );
            let id = pubky_switch_core::message::Message::parse(reply.json(), &[])
                .unwrap()
                .msg_id;
            next.send_plaintext(w.carol_id(), guest.app().cert(), &reply, &[shop.app_id()])
                .unwrap();
            let read = shop.app_mut().receive(None).await.unwrap();
            let kept = shop.app().protocol_scan("paykit", b"pr/").await.unwrap();
            assert_eq!(
                kept.iter().any(|(key, _)| key.ends_with(id.as_bytes())),
                delete_parent,
                "{kind}: {read:?}"
            );
            if !delete_parent {
                assert!(read.events.iter().any(|e| matches!(e, AppEvent::MessageDropped { reason, .. } if reason.contains("payer"))), "{kind}: {read:?}");
            }
        }

        let period = january_details(w.clock.now_ms() + crate::common::HOUR_MS).billing_period;
        let body = period_proof(&w, shop.app_id(), period).await;
        for flagged in [false, true] {
            if flagged {
                lists.flag(&w.carol_id());
            }
            let proof = w.sign_as(
                &w.carol,
                "guest",
                &w.alice,
                "paykit.payment_proof",
                vec![request.request_hash],
                body.clone(),
            );
            let id = pubky_switch_core::message::Message::parse(proof.json(), &[])
                .unwrap()
                .msg_id;
            next.send_plaintext(w.carol_id(), guest.app().cert(), &proof, &[shop.app_id()])
                .unwrap();
            shop.receive(None).await.unwrap();
            let kept = shop.app().protocol_scan("paykit", b"pr/").await.unwrap();
            assert_eq!(
                kept.iter().any(|(key, _)| key.ends_with(id.as_bytes())),
                !flagged
            );
        }
        assert_eq!(shop.spam_count(&w.carol_id()).await.unwrap(), 1);
        accept_activity_successor(&w, &mut shop, &mut phone, &target).await;
        shop.receive(None).await.unwrap();
        let sale = shop
            .payee_activity()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.request_hash == Some(request.request_hash))
            .unwrap();
        assert!(sale.periods.is_empty());
    }
}

#[tokio::test]
async fn test_provider_loss_credit_uses_the_signed_payer_of_a_disputed_period() {
    use crate::periods::{january_details, monthly};
    use paykit_lib::collections::Collection;
    use paykit_sdk::AppScopes;

    for (same_payer, delete_parent, dispute_period) in [
        (false, false, true),
        (true, false, true),
        (false, true, true),
        (false, true, false),
    ] {
        let w = World::new();
        let scopes = AppScopes::new().request().build().unwrap();
        let mut shop = w.join(&w.alice, "shop", scopes.clone()).await;
        let mut phone = w.join(&w.alice, "phone", scopes).await;
        let pay = AppScopes::new().pay().build().unwrap();
        let mut bob = w.join(&w.bob, "buyer", pay.clone()).await;
        let mut carol = w.join(&w.carol, "subscriber", pay).await;
        bob.app_mut().invite(&w.alice_id()).await.unwrap();
        carol.app_mut().invite(&w.alice_id()).await.unwrap();
        let now = w.clock.now_ms();
        let purchase = request_from(&mut shop, w.bob_id(), draft("1", now)).await;
        bob.receive(None).await.unwrap();
        assert!(matches!(
            bob.pay_request(&purchase, PayOptions::default())
                .await
                .outcome
                .unwrap(),
            PayOutcome::Paid { .. }
        ));
        shop.receive(None).await.unwrap();
        let family = Collection::PayeeActivity;
        let credit = shop
            .app()
            .record_key(&activity_target(&purchase, family))
            .unwrap();
        assert!(shop.verify_payment(&credit).await.unwrap().is_some());

        let subscriber = if same_payer { &mut bob } else { &mut carol };
        let subscription = request_from(&mut shop, subscriber.identity(), monthly(now, None)).await;
        subscriber.receive(None).await.unwrap();
        subscriber.accept_subscription(&subscription).await.unwrap();
        shop.receive(None).await.unwrap();
        let parent = activity_target(&subscription, family);
        let sale = shop.app().record_key(&parent).unwrap();
        let details = january_details(now + crate::common::HOUR_MS);
        let period = period_target(&subscription, family, &details.billing_period);
        shop.send_period_details(&sale, details.clone())
            .await
            .unwrap();
        let mut targets = vec![(&parent, delete_parent)];
        if dispute_period {
            targets.push((&period, false));
        }
        recover_activity_writes(&w, &mut shop, &mut phone, &targets).await;
        follow_loss(&mut shop).await;
        let verified = shop.verify_payment(&credit).await;
        if same_payer || delete_parent {
            // The same payer's divergent sale, or a divergent delete whose payer can't be
            // told, holds the credit for the user, also with the consent that the payment may
            // be credited already (Paykit spec 8.1 step 4).
            assert!(matches!(
                verified.unwrap_err().refusal(),
                Some(Refusal::MaybeCredited)
            ));
            let details = paykit_sdk::ReceiptDetails::default()
                .with_maybe_credited_confirmed(UserConfirmed::new());
            let refused = shop
                .confirm_payment_with(&credit, details)
                .await
                .unwrap_err();
            assert!(
                matches!(refused.refusal(), Some(Refusal::Divergent { records }) if records.iter().any(|r| r.record == sale && r.may_hold_credit)),
                "{refused:?}"
            );
        } else {
            // The other payer's disputed period doesn't withhold this purchase. Its
            // original method payment predates the provider loss and needs confirmation.
            assert!(matches!(
                verified.unwrap_err().refusal(),
                Some(Refusal::MaybeCredited)
            ));
            let details = paykit_sdk::ReceiptDetails::default()
                .with_maybe_credited_confirmed(UserConfirmed::new());
            assert!(shop
                .confirm_payment_with(&credit, details)
                .await
                .unwrap()
                .is_some());
        }
        for (target, deleted) in targets {
            let refused = shop
                .send_period_details(&sale, details.clone())
                .await
                .unwrap_err();
            let key = shop.app().record_key(target).unwrap();
            // Nothing is sent: over a divergent delete the sale is gone as the copy holds it;
            // over a divergent value the record is paused for the user (Paykit spec 6).
            if deleted {
                assert!(
                    matches!(refused.refusal(), Some(Refusal::NotFound)),
                    "{refused:?}"
                );
            } else {
                assert!(
                    matches!(refused.refusal(), Some(Refusal::Divergent { records }) if records.iter().any(|r| r.record == key)),
                    "{refused:?}"
                );
            }
            accept_activity_successor(&w, &mut shop, &mut phone, target).await;
        }
        confirm_reported_pauses(&mut shop).await;
        // The same period can take another signed set of details after both successors.
        shop.send_period_details(&sale, details).await.unwrap();
        if same_payer || delete_parent {
            // The held credit's way forward. Once successors clear the view and the user
            // confirmed what the list showed, the sale isn't listed at all. The credit goes
            // through the refusals the host shows and the user confirms, once (Paykit spec 6,
            // 8.1 step 4).
            let period_key = shop.app().record_key(&period).unwrap();
            assert!(shop
                .divergent_records()
                .await
                .unwrap()
                .iter()
                .all(|r| r.record != sale && r.record != period_key));
            assert_eq!(
                sale_status(&shop, &credit).await,
                paykit_sdk::PayeeStatus::ProofReceived
            );
            let consent = || {
                paykit_sdk::ReceiptDetails::default()
                    .with_maybe_credited_confirmed(UserConfirmed::new())
            };
            if same_payer {
                // The same payer's sale still holds it: its refusal names the sale, shown
                // for a credit. Over a delete, the successor names the other payer and lets
                // it go at once.
                let refused = shop
                    .confirm_payment_with(&credit, consent())
                    .await
                    .unwrap_err();
                assert!(
                    crate::divergent::shown_for_a_credit(&refused)
                        && matches!(refused.refusal(), Some(Refusal::Divergent { records }) if records.iter().any(|r| r.record == sale)),
                    "{refused:?}"
                );
            }
            let receipt = if same_payer {
                let (receipt, refused_divergent) = credit_as_a_host(&mut shop, &credit).await;
                assert_eq!(refused_divergent, 1);
                receipt
            } else {
                shop.confirm_payment_with(&credit, consent())
                    .await
                    .unwrap()
                    .expect("the successor names the other payer: the credit goes")
            };
            let again = shop.confirm_payment_with(&credit, consent()).await.unwrap();
            assert_eq!(again, Some(receipt));
            assert_eq!(
                sale_status(&shop, &credit).await,
                paykit_sdk::PayeeStatus::Receipted
            );
        }
        assert_eq!(executions(&w), 1);
    }
}

#[tokio::test]
async fn test_provider_loss_restore_keeps_its_held_answer_over_a_disputed_delete() {
    use crate::periods::queued;
    let w = World::new();
    let mut wallet = w
        .join(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new()
                .pay()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.block(&w.bob_id()).await.unwrap();
    let answer = wallet.reject_request(&request, None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = restored(&w, &backup).await;
    let target = request_target(&request);
    recover_activity_with_phone_write(&w, &mut wallet, &mut phone, &target, true).await;
    follow_loss(&mut wallet).await;
    // The delete is divergent and the user hasn't confirmed it: its upload may still hold the
    // request, so nothing is decided on it, and the answer stays held (Paykit spec 6).
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    let answers = wallet.app().protocol_scan("paykit", b"ha/").await.unwrap();
    assert!(
        answers
            .iter()
            .any(|(key, _)| key.ends_with(answer.as_bytes())),
        "{answers:?}"
    );
    assert!(wallet
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_some());
    assert!(!queued(&wallet, &answer).await);
    assert!(!wallet
        .notices()
        .await
        .unwrap()
        .iter()
        .any(|n| n.msg_id.as_deref() == Some(&answer)));
    // A successor brings the request back; with the user's confirmation, the answer goes.
    accept_activity_successor(&w, &mut wallet, &mut phone, &target).await;
    confirm_reported_pauses(&mut wallet).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ha/")
        .await
        .unwrap()
        .is_empty());
    assert!(wallet
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    assert!(crate::common::possibly_delivered(&wallet, &answer).await);
}

#[tokio::test]
async fn test_provider_loss_restore_waits_for_the_parent_before_noting_period_details_possibly_delivered(
) {
    use crate::periods::{january_details, monthly, queued};
    use paykit_lib::collections::Collection;
    use paykit_sdk::AppScopes;
    // The restore's check of the period details it sent waits while the parent may hold a
    // stop. Either way out ends the wait, a successor that shows no stop or the user's
    // confirmation of what the paused parent shows, and the details are then noted possibly
    // delivered: nothing is queued again without current evidence (Paykit spec 6, 7.2).
    for by_confirmation in [false, true] {
        let w = World::new();
        let scopes = AppScopes::new().request().build().unwrap();
        let mut shop = w.join(&w.alice, "shop", scopes.clone()).await;
        let mut phone = w.join(&w.alice, "phone", scopes).await;
        let mut buyer = w
            .join(&w.bob, "buyer", AppScopes::new().pay().build().unwrap())
            .await;
        buyer.app_mut().invite(&w.alice_id()).await.unwrap();
        let now = w.clock.now_ms();
        let subscription = request_from(&mut shop, w.bob_id(), monthly(now, None)).await;
        buyer.receive(None).await.unwrap();
        buyer.accept_subscription(&subscription).await.unwrap();
        shop.receive(None).await.unwrap();
        let target = activity_target(&subscription, Collection::PayeeActivity);
        let key = shop.app().record_key(&target).unwrap();
        let details = january_details(now + crate::common::HOUR_MS);
        let period = period_target(
            &subscription,
            Collection::PayeeActivity,
            &details.billing_period,
        );
        let sent = shop.send_period_details(&key, details).await.unwrap();
        let backup = shop.backup().await.unwrap();
        drop(shop);
        let mut shop = restored(&w, &backup).await;
        shop.app_mut().read_record(&period).await.unwrap().unwrap();
        recover_activity_with_phone_write(&w, &mut shop, &mut phone, &target, false).await;
        follow_loss(&mut shop).await;
        assert!(shop
            .app()
            .protocol_get("paykit", b"ro")
            .await
            .unwrap()
            .is_some());
        assert!(!queued(&shop, &sent.msg_id).await);
        assert!(!crate::common::possibly_delivered(&shop, &sent.msg_id).await);
        if by_confirmation {
            assert!(shop
                .divergent_records()
                .await
                .unwrap()
                .iter()
                .any(|r| r.record == key));
            confirm_reported_pauses(&mut shop).await;
        } else {
            accept_activity_successor(&w, &mut shop, &mut phone, &target).await;
        }
        shop.receive(None).await.unwrap();
        assert!(
            crate::common::possibly_delivered(&shop, &sent.msg_id).await,
            "by confirmation: {by_confirmation}"
        );
        assert!(!queued(&shop, &sent.msg_id).await);
        assert!(shop
            .app()
            .protocol_get("paykit", b"ro")
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn test_provider_loss_restore_reconciles_history_after_an_activity_successor() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("payment is in flight")
    };
    let obligation = wallet.payments().await.unwrap().remove(0).obligation;
    w.rail.settle(&attempt_id, true);
    drop(wallet);
    let mut wallet = restored(&w, &backup).await;
    let target = request_target(&request);
    wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert!(wallet.payments().await.unwrap().is_empty());
    recover_activity_with_phone_write(&w, &mut wallet, &mut phone, &target, false).await;
    follow_loss(&mut wallet).await;
    let report = wallet.reconcile().await.unwrap();
    assert!(
        report
            .doubtful
            .iter()
            .any(|d| d.attempts.contains(&attempt_id)),
        "{report:?}"
    );
    // The restored attempt waits on its divergent record, paid once (Paykit spec 6).
    assert!(
        report.resolved.iter().any(
            |p| matches!(p, PayOutcome::RecordDivergent { attempt_id: id, .. } if *id == attempt_id)
        ),
        "{report:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
    accept_activity_successor(&w, &mut wallet, &mut phone, &target).await;
    confirm_reported_pauses(&mut wallet).await;
    let report = wallet.reconcile().await.unwrap();
    assert!(
        report
            .resolved
            .iter()
            .any(|p| matches!(p,PayOutcome::Paid {attempt_id:id,..} if *id==attempt_id)),
        "{report:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_provider_loss_kept_and_stored_details_wait_for_each_disputed_record() {
    use crate::periods::{january_details, monthly};
    use pubky_switch_client::testing::Fault;
    use pubky_switch_core::http::ErrorCode;
    for (dispute_parent, confirmed) in [(true, false), (false, false), (true, true), (false, true)]
    {
        let w = World::new();
        let mut wallet = w.wallet("wallet").await;
        let mut phone = w.wallet("phone").await;
        let mut shop = w.shop().await;
        let now = w.clock.now_ms();
        let request = request_from(
            &mut shop,
            w.alice_id(),
            monthly(now, None).with_period_deadline_s(86_400),
        )
        .await;
        wallet.receive(None).await.unwrap();
        wallet.accept_subscription(&request).await.unwrap();
        shop.receive(None).await.unwrap();
        let sale = shop.payee_activity().await.unwrap()[0].record;
        let details = january_details(now + crate::common::HOUR_MS);
        shop.send_period_details(&sale, details.clone())
            .await
            .unwrap();
        wallet.receive(None).await.unwrap();
        let parent = request_target(&request);
        let period = period_target(
            &request,
            paykit_lib::collections::Collection::PayerActivity,
            &details.billing_period,
        );
        // An entry the service refused to write is already the SDK's responsibility.
        let parked = shop
            .send_period_details(&sale, details.clone())
            .await
            .unwrap()
            .msg_id;
        for _ in 0..2 {
            w.alice.switch().fail_next(Fault::Error {
                path: "/ops".into(),
                code: ErrorCode::PermissionMissing,
                current: serde_json::Value::Null,
            });
        }
        wallet.receive(None).await.unwrap();
        assert!(!wallet.app_mut().parked_entries().await.unwrap().is_empty());
        let target = if dispute_parent { &parent } else { &period };
        let next =
            recover_activity_with_phone_write(&w, &mut wallet, &mut phone, target, false).await;
        follow_loss(&mut wallet).await;
        // A later arrival is admitted directly into the ordinary kept-reply pool.
        let message = w.sign_as(
            &w.bob,
            "shop",
            &w.alice,
            "paykit.payment_period_details",
            vec![request.request_hash],
            details.to_body().unwrap(),
        );
        let id = pubky_switch_core::message::Message::parse(message.json(), &[])
            .unwrap()
            .msg_id;
        next.send_plaintext(w.bob_id(), shop.app().cert(), &message, &[wallet.app_id()])
            .unwrap();
        wallet.app_mut().receive(None).await.unwrap();
        // The details wait for the user's confirmation of the divergent record, held on the
        // feed, never written into the period record (Paykit spec 6); the notices below show
        // they are given up, not dropped, once they are 30 days old.
        let held = wallet
            .app_mut()
            .read_record(&period)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(held.content.value().unwrap()).contains(id.as_str()),
            "parent={dispute_parent}: the details wait over the divergent record"
        );
        wallet.receive(None).await.unwrap();
        assert!(!wallet
            .app()
            .protocol_scan("paykit", b"se/")
            .await
            .unwrap()
            .is_empty());
        if confirmed {
            // The user's way forward: once the user confirms what the divergent record
            // shows, the details kept over it are written into the period record.
            confirm_reported_pauses(&mut wallet).await;
            wallet.receive(None).await.unwrap();
            let held = wallet
                .app_mut()
                .read_record(&period)
                .await
                .unwrap()
                .unwrap();
            let stored = paykit_lib::records::ActivityRecord::read(
                held.content.value().unwrap(),
                &w.alice_id(),
                paykit_lib::records::ActivitySide::Payer,
            )
            .unwrap();
            let stored: Vec<String> = stored
                .details()
                .iter()
                .map(|details| details.message().msg_id.clone())
                .collect();
            for msg_id in [&parked, &id] {
                assert!(
                    stored.contains(msg_id),
                    "parent={dispute_parent}: the details are written once confirmed: {stored:?}"
                );
            }
            continue;
        }
        // The January record no longer takes these details. While its write is
        // disputed the original entries still have to be given up with their IDs.
        w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
        wallet.receive(None).await.unwrap();
        let notices = wallet.notices().await.unwrap();
        for msg_id in [&parked, &id] {
            assert!(
                notices.iter().any(|n| n.msg_id.as_ref() == Some(msg_id)
                    && n.kind == paykit_sdk::NoticeKind::MessageGivenUp),
                "parent={dispute_parent} {notices:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_provider_loss_another_apps_request_stays_in_custody_during_a_dispute() {
    use paykit_lib::collections::Collection;
    use paykit_lib::records::{ActivityRecord, ActivitySide};
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    phone.receive(None).await.unwrap();
    let target = request_target(&request);
    let sale = shop
        .app_mut()
        .read_record(&activity_target(&request, Collection::PayeeActivity))
        .await
        .unwrap()
        .unwrap();
    let sent = ActivityRecord::read(
        sale.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payee,
    )
    .unwrap()
    .request
    .unwrap();
    let next = recover_activity_with_phone_write(&w, &mut wallet, &mut phone, &target, true).await;
    follow_loss(&mut wallet).await;
    next.send_plaintext(
        w.bob_id(),
        shop.app().cert(),
        sent.plaintext(),
        &[wallet.app_id()],
    )
    .unwrap();
    let read = wallet.app_mut().receive(None).await.unwrap();
    let kept = wallet.app().protocol_scan("paykit", b"dw/").await.unwrap();
    assert_eq!(kept.len(), 1, "{read:?}");
    confirm_reported_pauses(&mut wallet).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.app().protocol_scan("paykit", b"dw/").await.unwrap(),
        kept
    );
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    let now = w.clock.now_ms();
    next.push_missed(1, now - 1_000, now);
    next.fail_next(pubky_switch_client::testing::Fault::Unreachable {
        path: "records?collection".into(),
        after: 3,
    });
    let read = wallet.receive(None).await.unwrap();
    assert!(read.records_unlisted.is_some(), "{read:?}");
    // A delete the copy holds while it may lack records decides nothing (Paykit spec 8.1,
    // "Records not listed yet"): the request stays kept, neither written nor given up.
    assert_eq!(
        wallet.app().protocol_scan("paykit", b"dw/").await.unwrap(),
        kept
    );
    // Listed again, it is given up with the host's notice, 30 days old.
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"dw/")
        .await
        .unwrap()
        .is_empty());
    assert!(wallet
        .notices()
        .await
        .unwrap()
        .iter()
        .any(|n| n.msg_id.as_ref() == Some(&sent.message().msg_id)
            && n.kind == paykit_sdk::NoticeKind::MessageGivenUp));
}

#[tokio::test]
async fn test_provider_loss_missing_record_epoch_holds_the_original_request_proof_and_receipt() {
    use crate::unrequested::{store_bundle, store_bundle_at, store_bundle_from};
    use paykit_lib::collections::Collection;
    use paykit_lib::records::{ActivityRecord, ActivitySide};
    use paykit_sdk::AppScopes;
    use pubky_switch_client::app::{RecordContent, Unopened};

    let w = World::new();
    let both = AppScopes::new()
        .pay()
        .request()
        .manage_contacts()
        .build()
        .unwrap();
    let mut writer = w.join(&w.alice, "writer", both.clone()).await;
    let mut phone = w.join(&w.alice, "phone", both.clone()).await;
    let mut buyer = w
        .join(&w.bob, "buyer", AppScopes::new().pay().build().unwrap())
        .await;
    let mut seller = w.shop().await;
    buyer.app_mut().invite(&w.alice_id()).await.unwrap();
    writer.app_mut().invite(&w.bob_id()).await.unwrap();
    let now = w.clock.now_ms();
    let sold = request_from(&mut writer, w.bob_id(), draft("1", now)).await;
    buyer.receive(None).await.unwrap();
    buyer
        .pay_request(&sold, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let sent = seller
        .send_request(w.alice_id(), draft("2", now))
        .await
        .unwrap();
    assert!(matches!(
        sent.delivery,
        paykit_sdk::MessageDelivery::Accepted { .. }
    ));
    let bought = sent.request.unwrap();
    crate::common::receive_message(&mut writer, &sent.msg_id).await;
    writer
        .pay_request(&bought, PayOptions::default())
        .await
        .outcome
        .unwrap();
    seller.receive(None).await.unwrap();
    let seller_target = activity_target(&bought, Collection::PayeeActivity);
    let sale = seller.app().record_key(&seller_target).unwrap();
    seller.confirm_payment(&sale).await.unwrap().unwrap();
    let from_seller = seller
        .app_mut()
        .read_record(&seller_target)
        .await
        .unwrap()
        .unwrap();
    let from_buyer = buyer
        .app_mut()
        .read_record(&request_target(&sold))
        .await
        .unwrap()
        .unwrap();
    let from_seller = ActivityRecord::read(
        from_seller.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payee,
    )
    .unwrap();
    let from_buyer = ActivityRecord::read(
        from_buyer.content.value().unwrap(),
        &w.bob_id(),
        ActivitySide::Payer,
    )
    .unwrap();
    let messages = [
        from_buyer.proof.unwrap(),
        from_seller.request.unwrap(),
        from_seller.receipt.unwrap(),
    ];
    let sale_target = activity_target(&sold, Collection::PayeeActivity);
    let purchase_target = request_target(&bought);
    let targets = [(&sale_target, false), (&purchase_target, false)];

    // The observer joins after these messages were sent. Its next bundle will
    // span only the current epoch, while the two other apps write at the previous one.
    let lists = Arc::new(crate::common::ListsStore::default());
    let mut observer = w
        .join_with(
            &w.alice,
            "observer",
            both,
            PaykitConfig::default(),
            lists.clone(),
        )
        .await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let sealed_at = w.alice.switch().identity_doc().unwrap().data_epoch();
    for app in [&mut writer, &mut phone] {
        store_bundle(&w, app.app_id(), app.app().cert());
        app.app_mut().refresh_identity().await.unwrap();
        app.app_mut().fetch_key_bundle().await.unwrap();
    }
    let writes = competing_activity_writes(&w, &mut writer, &mut phone, &targets).await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let current = w.alice.switch().identity_doc().unwrap().data_epoch();
    store_bundle(&w, phone.app_id(), phone.app().cert());
    phone.app_mut().refresh_identity().await.unwrap();
    phone.app_mut().fetch_key_bundle().await.unwrap();
    let cert = observer.app().cert().clone();
    store_bundle_from(&w, observer.app_id(), &cert, current);
    observer.app_mut().refresh_identity().await.unwrap();
    observer.app_mut().fetch_key_bundle().await.unwrap();
    for &(target, _) in &targets {
        let held = observer
            .app_mut()
            .read_record(target)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(held.content, RecordContent::Unreadable(Unopened::KeyNotHeld { data_epoch }) if data_epoch == sealed_at),
            "older epoch {sealed_at}, current {current}: {held:?}"
        );
    }
    let next = recover_competing_writes(&w, &mut observer, &mut phone, &targets, writes).await;
    for &(target, _) in &targets {
        let held = observer
            .app_mut()
            .read_record(target)
            .await
            .unwrap()
            .unwrap();
        assert!(held.divergent && !held.different_write_at_held_version);
        assert!(
            matches!(held.content, RecordContent::Unreadable(Unopened::KeyNotHeld { data_epoch }) if data_epoch==sealed_at),
            "older epoch {sealed_at}, current {current}: {held:?}"
        );
        assert!(observer
            .app()
            .scope_key(current, &target.collection.policy.read_scope)
            .is_some());
    }
    follow_loss(&mut observer).await;
    lists.flag(&w.bob_id());
    for m in &messages {
        next.send_plaintext(w.bob_id(), m.cert(), m.plaintext(), &[observer.app_id()])
            .unwrap();
    }
    for _ in 0..2 {
        let read = observer.receive(None).await.unwrap();
        assert_eq!(
            read.key_epochs_awaited,
            [sealed_at],
            "older epoch {sealed_at}, current {current}: {read:?}"
        );
        assert!(read
            .applied
            .iter()
            .all(|id| !messages.iter().any(|m| &m.message().msg_id == id)));
        assert_eq!(observer.spam_count(&w.bob_id()).await.unwrap(), 0);
        for pool in [b"pr/", b"dw/"] {
            assert!(observer
                .app()
                .protocol_scan("paykit", pool)
                .await
                .unwrap()
                .is_empty());
        }
    }
    // A payee's cancellation needs only its request key to stop the payer.
    let stop = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_request_cancellation",
        vec![bought.request_hash],
        Default::default(),
    );
    let stop_id = pubky_switch_core::message::Message::parse(stop.json(), &[])
        .unwrap()
        .msg_id;
    next.send_plaintext(w.bob_id(), seller.app().cert(), &stop, &[observer.app_id()])
        .unwrap();
    let stopped = observer.receive(None).await.unwrap();
    assert!(stopped.applied.contains(&stop_id), "{stopped:?}");
    assert_eq!(stopped.key_epochs_awaited, [sealed_at]);
    let stop_key = format!(
        "kc/{}/{}/{:020}",
        pubky_switch_core::enc::b64(&observer.app().record_key(&purchase_target).unwrap()),
        pubky_switch_core::enc::b64(&seller.app().cert().hash()),
        observer.app().own_identity().generation(),
    );
    assert!(observer
        .app()
        .protocol_get("paykit", stop_key.as_bytes())
        .await
        .unwrap()
        .is_some());
    // The same verified inbox entries return when their record epoch comes.
    store_bundle_at(&w, &next, observer.app_id(), &cert, 1);
    let mut applied = Vec::new();
    for _ in 0..4 {
        applied.extend(observer.receive(None).await.unwrap().applied);
    }
    for message in &messages {
        assert_eq!(
            applied
                .iter()
                .filter(|id| **id == message.message().msg_id)
                .count(),
            1,
            "{applied:?}"
        );
    }
    assert_eq!(observer.spam_count(&w.bob_id()).await.unwrap(), 0);
    // Read now, the records show what arrived before the loss, and each is paused for the
    // user (Paykit spec 6): the proof the sale record holds already, and the receipt kept
    // over the paused purchase record. Neither is dropped.
    let mut kept = Vec::new();
    let mut held = String::new();
    for &(target, _) in &targets {
        kept.extend(kept_over(&observer, &observer.app().record_key(target).unwrap()).await);
        let record = observer
            .app_mut()
            .read_record(target)
            .await
            .unwrap()
            .unwrap();
        held.push_str(&String::from_utf8_lossy(record.content.value().unwrap()));
    }
    for message in [&messages[0], &messages[2]] {
        let id = message.message().msg_id.as_str();
        assert!(
            kept.iter().any(|(key, _)| key.ends_with(id)) || held.contains(id),
            "{kept:?}"
        );
    }
    assert!(observer
        .divergent_records()
        .await
        .unwrap()
        .iter()
        .all(|shown| shown.paused));
}

#[tokio::test]
async fn test_provider_loss_the_final_activity_read_precedes_the_payment_method_call() {
    use crate::blocked_return::PayingStore;
    use crate::common::{btc, endpoint, list_version};
    use paykit_sdk::UnrequestedPayment;
    let w = World::new();
    let disk = Arc::new(PayingStore::default());
    let mut wallet = w.wallet_on("wallet", disk.clone()).await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let payment = || {
        UnrequestedPayment::new(w.bob_id(), btc("0.0015"))
            .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")))
    };
    // The first checks succeeded. At the Paying commit the app's next read of
    // this payer activity becomes unavailable, before the call to the method: the
    // store's failure is the call's error, and nothing is paid.
    let at_commit = disk.clone();
    disk.on_paying(move || at_commit.fail_next_payer_activity_read());
    let failed = wallet
        .pay_without_request(payment())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            failed,
            PaykitError::Client(pubky_switch_client::app::AppError::Store(_))
        ),
        "{failed:?}"
    );
    assert_eq!(executions(&w), 0);
    wallet.resume_payments().await.unwrap();
    assert_eq!(executions(&w), 0);
    assert!(matches!(
        wallet.pay_without_request(payment()).await.outcome.unwrap(),
        PayOutcome::Paid { .. }
    ));
    assert_eq!(executions(&w), 1);
}

/// Whether `outcome` is the refusal of an automatic payment under the shared post-loss hold
/// (Paykit spec 8.1).
pub(super) fn held_after_loss(outcome: &paykit_sdk::Result<PayOutcome>) -> bool {
    matches!(
        outcome.as_ref().err().and_then(PaykitError::refusal),
        Some(Refusal::NotAutomatic(why)) if why.contains("provider loss")
    )
}

/// The doubt a payment was refused with, as [`Refusal::MayBePaid`] shows it.
pub(crate) fn doubt_of(paid: &paykit_sdk::Result<PayOutcome>) -> PaymentDoubt {
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

/// The status of the payee activity record `record` that `shop` shows.
async fn sale_status(shop: &Paykit, record: &[u8; 32]) -> paykit_sdk::PayeeStatus {
    shop.payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|sale| sale.record == *record)
        .expect("the shop shows the sale")
        .status
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
    // An automatic payment of a request from before the loss is refused, under the shared
    // post-loss hold, a confirmation of its doubt passing nothing.
    let refused = wallet
        .pay_request_automatically(&held, PayOptions::default())
        .await
        .outcome;
    assert!(held_after_loss(&refused), "{refused:?}");
    let doubt = doubt_of(
        &wallet
            .pay_request(&held, PayOptions::default())
            .await
            .outcome,
    );
    assert!(doubt.before_loss.is_some(), "{doubt:?}");
    let refused = wallet
        .pay_request_automatically(
            &held,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .outcome;
    assert!(held_after_loss(&refused), "{refused:?}");
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
async fn test_provider_loss_consent_to_one_attempt_pays_it_once_and_states_nothing() {
    // Paykit spec 7.3, "Consent to one attempt": shown that the request may already have
    // been paid before the loss, the user consents to one attempt. It is made once. The
    // consent states nothing: no statement is kept, and the doubt ends only because the
    // claim at the new service is SETTLED. The same consent pays nothing more.
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
    in_doubt_of_the_loss(&mut wallet, &request, &doubt).await;
    assert_eq!(executions(&w), 0);
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt.clone()),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 1);
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"pn/")
        .await
        .unwrap()
        .is_empty());
    let claim = wallet
        .app_mut()
        .read_claim(&doubt.obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Settled);
    assert_eq!(status(&wallet, &request).await, RequestStatus::Paid);
    let again = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
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
async fn test_provider_loss_nothing_the_user_states_releases_an_automatic_payment() {
    // Paykit spec 7.3: whatever the user confirms or states, an obligation from before the
    // loss is never paid automatically: the shared post-loss hold refuses it (Paykit spec
    // 8.1). A payment the method made before the loss, its answer lost, is resolved from the
    // method's history and not paid again. The user's statement that a request wasn't paid
    // before the loss lets a payment by hand of it go as any other.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let in_flight = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // Before the loss, the method is called for the second request and its answer is lost;
    // the method's history knows it was paid.
    w.rail.set_idempotent(true);
    w.rail.next(Behaviour::LoseAnswer);
    let lost = wallet
        .pay_request(&in_flight, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(!matches!(lost, PayOutcome::Paid { .. }), "{lost:?}");
    assert_eq!(executions(&w), 1);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    let doubt = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    // Before any statement, the shared post-loss hold refuses an automatic payment.
    let refused = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome;
    assert!(held_after_loss(&refused), "{refused:?}");
    wallet
        .confirm_not_paid_before_loss(
            &doubt.obligation,
            &doubt.before_loss.unwrap(),
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    // The statement is kept for that request alone, which reads open again.
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"pn/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(status(&wallet, &request).await, RequestStatus::Open);
    for options in [
        PayOptions::default(),
        PayOptions::default().with_may_be_paid_confirmed(doubt.clone()),
    ] {
        let refused = wallet
            .pay_request_automatically(&request, options)
            .await
            .outcome;
        assert!(held_after_loss(&refused), "{refused:?}");
    }
    // The payment from before the loss is resolved from the method's history: nothing is paid
    // again.
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        !resumed
            .iter()
            .any(|outcome| matches!(outcome, PayOutcome::Paid { .. })),
        "{resumed:?}"
    );
    assert_eq!(executions(&w), 1);
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 2);
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
    assert!(held_after_loss(&refused), "{refused:?}");
    let doubt = doubt_of(
        &wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome,
    );
    assert!(doubt.before_loss.is_some(), "{doubt:?}");
    let refused = wallet
        .pay_request_automatically(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .outcome;
    assert!(held_after_loss(&refused), "{refused:?}");
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
        RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true),
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
    declare_second_loss(&w);
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
        matches!(refused.refusal(), Some(Refusal::Usage(_))),
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
    declare_second_loss(&w);
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
    let next = declare_second_loss(w);
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
    declare_second_loss(&w);
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
        paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
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

#[tokio::test]
async fn test_provider_loss_keeps_the_floor_divergence_hold() {
    // Paykit wire 5.6, Divergent floor: the hold survives a later provider loss.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "block_floor",
            paykit_lib::collections::Collection::BlockFloor.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::block_floor(&w.alice_id()),
    );
    wallet
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    w.alice
        .switch()
        .make_divergent([0x5a; 16], key, [1; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    let held = |bytes: Option<Vec<u8>>| {
        let value: serde_json::Value = serde_json::from_slice(&bytes.unwrap()).unwrap();
        value["d"]["divergent"] == serde_json::json!(true)
    };
    assert!(held(
        wallet.app().protocol_get("paykit", b"fk").await.unwrap()
    ));
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    follow_loss(&mut wallet).await;
    assert!(held(
        wallet.app().protocol_get("paykit", b"fk").await.unwrap()
    ));
}

/// The generation `app`'s kept floor (`fk`) shows the record live in.
async fn floor_live_in(app: &Paykit) -> Option<u64> {
    let bytes = app.app().protocol_get("paykit", b"fk").await.unwrap()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["d"]["live_in"].as_u64()
}

#[tokio::test]
async fn test_provider_loss_floor_upload_seen_only_in_a_listing_shows_no_generation() {
    // Paykit wire 5.6, Seen floor and permanence: after a loss the wallet uploads the floor to
    // the new service. The phone, restored on a fresh device from a backup taken before the
    // floor, sees that upload in its listing first. An unauthenticated copy shows a generation
    // only from a read the current service answered then, so after the listing alone the
    // phone's kept floor shows none, and an absence then raises no alarm from it. The highest floor seen still holds:
    // a one-time request created past the loss's window and dated before that floor isn't
    // paid automatically.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let before = phone.backup().await.unwrap();
    drop(phone);
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "block_floor",
            paykit_lib::collections::Collection::BlockFloor.fixed_policy(),
        ),
        &paykit_lib::derive::natural_key::block_floor(&w.alice_id()),
    );
    let floor = w.clock.now_ms() + 7 * 24 * 3_600_000;
    wallet
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(floor)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    w.clock.advance_ms(60_000);
    let next = declare_loss(&w);
    let until = follow_loss(&mut wallet).await;
    let mut phone = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &before,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    // The phone's own reads of the floor fail meanwhile, so only its listing shows the upload.
    let path = format!(
        "/records/{}",
        pubky_switch_core::enc::b64(&phone.app().record_key(&target).unwrap())
    );
    const FAILED_READS: usize = 40;
    for _ in 0..FAILED_READS {
        next.fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: path.clone(),
            after: 0,
        });
    }
    follow_generation(&mut phone).await;
    // Leftover failures are spent on reads that show nothing to Paykit.
    for _ in 0..FAILED_READS {
        if phone.app_mut().read_record(&target).await.is_ok() {
            break;
        }
    }
    let held = phone
        .app()
        .shared_record(&phone.app().record_key(&target).unwrap())
        .await
        .unwrap()
        .expect("the upload, listed");
    assert!(!held.authenticated);
    assert_eq!(floor_live_in(&phone).await, None);
    follow_loss(&mut phone).await;
    w.clock
        .advance_ms(until.saturating_sub(w.clock.now_ms()) + 3_600_000);
    assert!(w.clock.now_ms() < floor);
    let request = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    phone.receive(None).await.unwrap();
    let outcome = phone
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome;
    assert!(
        !matches!(outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{outcome:?}"
    );
    assert!(w.rail.all_pay_calls().is_empty());
}

/// Alice's wallet dispatches a payment the method leaves pending, is backed up with it in
/// flight and lost; the signer declares a provider loss, which the laptop follows, so the new
/// service holds the request's record but no claim for the obligation, and the wallet is
/// restored from that backup there. `paid` settles the dispatch paid before the restore;
/// otherwise the method's history, which doesn't survive a restore, tells nothing of it.
/// Returns the world, the restored wallet, the shop, the request and the attempt.
async fn restored_dispatch_without_a_claim(
    paid: bool,
) -> (World, Paykit, Paykit, RequestRef, paykit_sdk::AttemptId) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut laptop = w.wallet("laptop").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    laptop.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let pending = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = pending else {
        panic!("pending: {pending:?}")
    };
    let backup = wallet.backup().await.unwrap();
    if paid {
        w.rail.settle(&attempt_id, true);
    } else {
        w.rail.set_declaration(
            RailDeclaration::new(
                paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
                paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
            )
            .with_history(true),
        );
        w.rail.forget_history(&attempt_id);
    }
    drop(wallet);
    w.clock.advance_ms(60_000);
    declare_loss(&w);
    // The laptop follows the loss first: the request's record reaches the new service.
    follow_loss(&mut laptop).await;
    let mut app = restored(&w, &backup).await;
    follow_loss(&mut app).await;
    app.receive(None).await.unwrap();
    let obligation = app.payments().await.unwrap()[0].obligation;
    assert!(
        app.app_mut()
            .read_claim(&obligation)
            .await
            .unwrap()
            .is_none(),
        "the new service holds no claim for it"
    );
    (w, app, shop, request, attempt_id)
}

/// The stage of `attempt_id`'s journal entry.
async fn stage_of(app: &Paykit, attempt_id: &paykit_sdk::AttemptId) -> paykit_sdk::Stage {
    app.payments()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.attempt_id == *attempt_id)
        .expect("the payment is journaled")
        .stage
}

#[tokio::test]
async fn test_provider_loss_restored_dispatch_without_a_claim_the_method_shows_paid_keeps_its_proof(
) {
    // The method's history shows the restored dispatch paid: it is recorded paid, its proof
    // reaches the payee, and nothing pays it again, even once the user states it wasn't paid
    // before the loss (Paykit spec 7.2, 8.4).
    let (w, mut app, mut shop, request, attempt_id) = restored_dispatch_without_a_claim(true).await;
    app.resume_payments().await.unwrap();
    assert_eq!(
        stage_of(&app, &attempt_id).await,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(status(&app, &request).await, RequestStatus::Paid);
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        paykit_sdk::PayeeStatus::ProofReceived
    );
    let obligation = app.payments().await.unwrap()[0].obligation;
    let loss = kept_loss(&app).await;
    app.confirm_not_paid_before_loss(&obligation, &loss, UserConfirmed::new())
        .await
        .unwrap();
    let options = crate::common::over_hold(&app, &request).await;
    let again = app.pay_request(&request, options).await.outcome;
    assert_eq!(
        again.as_ref().err().and_then(PaykitError::refusal),
        Some(&Refusal::AlreadyPaid),
        "{again:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_restored_dispatch_without_a_claim_or_history_stays_doubtful() {
    // Neither the claim nor the method's history can tell: the restored dispatch stays open
    // as potentially paid. The refusal names it as this app's own restored attempt, before
    // and after the user states the request wasn't paid before the loss, and nothing is
    // paid again until the user decides on it (Paykit spec 7.2, 8.4).
    let (w, mut app, _shop, request, attempt_id) = restored_dispatch_without_a_claim(false).await;
    app.resume_payments().await.unwrap();
    let stage = stage_of(&app, &attempt_id).await;
    assert!(
        !matches!(
            stage,
            paykit_sdk::Stage::Abandoned | paykit_sdk::Stage::Finished
        ),
        "{stage:?}"
    );
    let options = crate::common::over_hold(&app, &request).await;
    let refused = app.pay_request(&request, options.clone()).await.outcome;
    let doubt = doubt_of(&refused);
    assert_eq!(doubt.restored, vec![attempt_id], "{refused:?}");
    assert!(doubt.before_loss.is_some(), "{refused:?}");
    let obligation = app.payments().await.unwrap()[0].obligation;
    let loss = kept_loss(&app).await;
    app.confirm_not_paid_before_loss(&obligation, &loss, UserConfirmed::new())
        .await
        .unwrap();
    let again = app.pay_request(&request, options.clone()).await.outcome;
    assert_eq!(doubt_of(&again).restored, vec![attempt_id], "{again:?}");
    assert_eq!(executions(&w), 1);
    // The user's decision on it pays: seen "may already be paid", the user pays anyway.
    let shown = doubt_of(&again);
    let paid = app
        .pay_request(&request, options.with_may_be_paid_confirmed(shown))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 2);
}

/// The restored dispatch of [`restored_dispatch_without_a_claim`], its method silent,
/// resolved by the user as `resolution` after a resume left it open.
async fn resolved_without_a_claim(
    resolution: paykit_sdk::Resolution,
) -> (World, Paykit, RequestRef, paykit_sdk::AttemptId) {
    let (w, mut app, _shop, request, attempt_id) = restored_dispatch_without_a_claim(false).await;
    app.resume_payments().await.unwrap();
    let obligation = app.payments().await.unwrap()[0].obligation;
    app.resolve_attempt(&obligation, attempt_id, resolution, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        stage_of(&app, &attempt_id).await,
        paykit_sdk::Stage::Finished
    );
    (w, app, request, attempt_id)
}

/// Whether `app`'s journal still holds `attempt_id` once the clock is past the claim's
/// window and a resume ran on a verified service time there.
async fn journaled_past_the_window(
    w: &World,
    app: &mut Paykit,
    attempt_id: &paykit_sdk::AttemptId,
) -> bool {
    w.clock
        .advance_ms(2 * 24 * crate::common::HOUR_MS + pubky_switch_core::claims::RECONCILIATION_MS);
    app.receive(None).await.unwrap();
    app.resume_payments().await.unwrap();
    app.payments()
        .await
        .unwrap()
        .iter()
        .any(|p| p.attempt_id == *attempt_id)
}

#[tokio::test]
async fn test_provider_loss_restored_dispatch_without_a_claim_the_user_resolves_paid() {
    // No claim shows the restored dispatch and the method can't tell: the user, seen "may
    // already be paid", resolves it as paid. Its journal entry finishes, the request
    // record's `status` is `paid`, and with no claim to settle the journal keeps the user's
    // word: the request is refused as already paid, also once the user states it wasn't paid
    // before the loss (Paykit spec 7.2, 8.4). Past the claim's window the entry is dropped.
    let (w, mut app, request, attempt_id) =
        resolved_without_a_claim(paykit_sdk::Resolution::Done).await;
    let key = app.request(&request).await.unwrap().record;
    let record = app.app().shared_record(&key).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(record.content.value().unwrap()).unwrap();
    assert_eq!(value["status"], "paid", "{value}");
    let obligation = app.payments().await.unwrap()[0].obligation;
    let loss = kept_loss(&app).await;
    app.confirm_not_paid_before_loss(&obligation, &loss, UserConfirmed::new())
        .await
        .unwrap();
    let options = crate::common::over_hold(&app, &request).await;
    let refused = app.pay_request(&request, options).await.outcome;
    assert_eq!(
        refused.as_ref().err().and_then(PaykitError::refusal),
        Some(&Refusal::AlreadyPaid),
        "{refused:?}"
    );
    assert_eq!(executions(&w), 1);
    assert!(!journaled_past_the_window(&w, &mut app, &attempt_id).await);
}

#[tokio::test]
async fn test_provider_loss_restored_dispatch_without_a_claim_the_user_resolves_not_paid() {
    // The user resolves the same restored dispatch as not paid: its entry finishes, the
    // doubt no longer names it, and a payment the user then makes over the loss is made;
    // past the claim's window the resolved entry is dropped (Paykit spec 7.2).
    let (w, mut app, request, attempt_id) =
        resolved_without_a_claim(paykit_sdk::Resolution::Failed).await;
    let options = crate::common::over_hold(&app, &request).await;
    let refused = app.pay_request(&request, options.clone()).await.outcome;
    let doubt = doubt_of(&refused);
    assert!(doubt.restored.is_empty(), "{refused:?}");
    let paid = app
        .pay_request(&request, options.with_may_be_paid_confirmed(doubt))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(executions(&w), 2);
    assert!(!journaled_past_the_window(&w, &mut app, &attempt_id).await);
}

#[tokio::test]
async fn test_provider_loss_restored_dispatch_left_open_beside_a_new_one_restores_again() {
    // The user pays over the restored dispatch's doubt, and the method leaves the new payment
    // pending: two open entries for the obligation, one restored. A backup of that restores,
    // and both attempts are in the doubt the restored app shows (Paykit spec 7.2, 12.4).
    let (w, mut app, _shop, request, attempt_id) = restored_dispatch_without_a_claim(false).await;
    app.resume_payments().await.unwrap();
    let options = crate::common::over_hold(&app, &request).await;
    let refused = app.pay_request(&request, options.clone()).await.outcome;
    w.rail.next(Behaviour::Hang);
    let pending = app
        .pay_request(
            &request,
            options.with_may_be_paid_confirmed(doubt_of(&refused)),
        )
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending {
        attempt_id: newer, ..
    } = pending
    else {
        panic!("pending: {pending:?}")
    };
    let backup = app.backup().await.unwrap();
    drop(app);
    let mut again = restored(&w, &backup).await;
    again.receive(None).await.unwrap();
    let mut both = vec![attempt_id, newer];
    both.sort();
    let mut restored_open: Vec<_> = again
        .payments()
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.restored)
        .map(|p| p.attempt_id)
        .collect();
    restored_open.sort();
    assert_eq!(restored_open, both);
}

#[tokio::test]
async fn test_provider_loss_restored_dispatch_paid_whose_outcome_isnt_written_is_already_paid() {
    // As in the restored dispatch the method shows paid, but the outcome's write at the new
    // service doesn't go through: the entry holds the method's proof, unwritten. It is
    // already paid, never a payment the user may confirm again, and the user's `done`
    // writes its outcome with the proof (Paykit spec 7.2).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut laptop = w.wallet("laptop").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    laptop.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("pending");
    };
    let backup = wallet.backup().await.unwrap();
    w.rail.settle(&attempt_id, true);
    drop(wallet);
    w.clock.advance_ms(60_000);
    let next = declare_loss(&w);
    follow_loss(&mut laptop).await;
    let mut app = restored(&w, &backup).await;
    follow_generation(&mut app).await;
    // The outcome's write at the new service fails during reconciliation.
    for _ in 0..2 {
        next.fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    }
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    assert_eq!(
        stage_of(&app, &attempt_id).await,
        paykit_sdk::Stage::RailPaid
    );
    let obligation = app.payments().await.unwrap()[0].obligation;
    let options = crate::common::over_hold(&app, &request).await;
    let again = app.pay_request(&request, options).await.outcome;
    assert_eq!(
        again.as_ref().err().and_then(PaykitError::refusal),
        Some(&Refusal::AlreadyPaid),
        "{again:?}"
    );
    // Holding its proof, it is never marked `unknown`.
    let unknown = app
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Unknown,
            UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        unknown.refusal(),
        Some(&Refusal::AlreadyPaid),
        "{unknown:?}"
    );
    app.resolve_attempt(
        &obligation,
        attempt_id,
        paykit_sdk::Resolution::Done,
        UserConfirmed::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        stage_of(&app, &attempt_id).await,
        paykit_sdk::Stage::Finished
    );
    // Its proof goes with the outcome's batch, sent at the next flush.
    app.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        paykit_sdk::PayeeStatus::ProofReceived
    );
    assert_eq!(executions(&w), 1);
}

#[tokio::test]
async fn test_provider_loss_competing_upload_is_divergent_and_the_receipt_waits_for_the_user() {
    // An honest lost service: another app's write, never applied there, built on the same
    // base as the wallet's applied write, is uploaded after the loss. The wallet's proved
    // write stays current and the other is divergent (Pubky Switch wire 7.4.14). A receipt
    // for the paid request then waits for the user's confirmation of the record, nothing is
    // paid again, and once the user confirms what it shows, the receipt is taken.
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
    let writes = competing_activity_writes(&w, &mut wallet, &mut phone, &[(&target, false)]).await;
    let _next =
        recover_competing_writes(&w, &mut wallet, &mut phone, &[(&target, false)], writes).await;
    let key = wallet.app().record_key(&target).unwrap();
    follow_loss(&mut wallet).await;
    let shown = wallet
        .divergent_records()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.record == key)
        .unwrap();
    assert!(shown.paused && shown.divergent);
    w.clock.advance_ms(11 * 60_000);
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let receipt = shop.confirm_payment(&sale).await.unwrap().unwrap();
    shop.app_mut().flush_message(&receipt).await.unwrap();
    wallet.receive(None).await.unwrap();
    // Paused: the receipt waits, and nothing is paid again.
    let view = wallet.request(&request).await.unwrap();
    assert!(view.divergent && view.receipt.is_none(), "{view:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
    // The user confirms the record as shown: the receipt is taken, still with one payment.
    wallet
        .confirm_divergent(&shown, UserConfirmed::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert!(!view.divergent && view.receipt.is_some(), "{view:?}");
    assert_eq!(view.status, paykit_sdk::RequestStatus::Paid);
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// The honest case after a provider loss: another app's write on the same base as the
/// wallet's applied write, never applied at the lost service, uploaded after the loss; the
/// wallet's proved write stays current and the other is divergent (Pubky Switch wire 7.4.14).
async fn recover_with_divergent_phone_write(
    w: &World,
    wallet: &mut Paykit,
    phone: &mut Paykit,
    target: &pubky_switch_client::app::RecordTarget,
) -> Arc<ScriptedSwitch> {
    recover_with_divergent_phone_writes(w, wallet, phone, &[(target, false)]).await
}

/// [`recover_with_divergent_phone_write`] over each of `targets`.
async fn recover_with_divergent_phone_writes(
    w: &World,
    wallet: &mut Paykit,
    phone: &mut Paykit,
    targets: &[(&pubky_switch_client::app::RecordTarget, bool)],
) -> Arc<ScriptedSwitch> {
    let writes = competing_activity_writes(w, wallet, phone, targets).await;
    recover_competing_writes(w, wallet, phone, targets, writes).await
}

#[tokio::test]
async fn test_provider_loss_competing_upload_is_divergent_and_the_completed_payment_waits_for_the_user(
) {
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
    let _next = recover_with_divergent_phone_write(&w, &mut wallet, &mut phone, &target).await;
    let key = wallet.app().record_key(&target).unwrap();
    follow_loss(&mut wallet).await;
    w.rail.settle(&attempt_id, true);
    w.rail.set_declaration(
        RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true)
        .with_history(true),
    );
    // Divergent: the completed payment's record write waits, and it is not paid again.
    let held = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(held.as_slice(), [PayOutcome::RecordDivergent { .. }]),
        "{held:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
    // The user confirms the record as shown: the payment finishes, still paid once.
    let shown = wallet
        .divergent_records()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.record == key)
        .unwrap();
    wallet
        .confirm_divergent(&shown, UserConfirmed::new())
        .await
        .unwrap();
    let finished = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(finished.as_slice(), [PayOutcome::Paid { .. }]),
        "{finished:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
    let view = wallet.request(&request).await.unwrap();
    assert!(view.status == paykit_sdk::RequestStatus::Paid && !view.divergent);
}

#[tokio::test]
async fn test_provider_loss_competing_upload_is_divergent_and_the_credit_waits_for_the_user() {
    // The shop sells to bob once and on a subscription. After the loss, another app's write
    // on the subscription's sale record is uploaded and stays divergent, so the purchase's
    // credit can't tell that record holds no receipt for its settlement. The credit is
    // refused, naming that record as shown for a credit; once the user confirms exactly
    // what the refusal shows, the credit goes through with one receipt, and a second
    // confirmation sends none (Paykit spec 6 and 8.1 step 4).
    use crate::periods::{january_details, monthly};
    use paykit_lib::collections::Collection;
    use paykit_sdk::{AppScopes, ReceiptDetails};
    let w = World::new();
    let scopes = AppScopes::new().request().build().unwrap();
    let mut shop = w.join(&w.alice, "shop", scopes.clone()).await;
    let mut phone = w.join(&w.alice, "phone", scopes).await;
    let pay = AppScopes::new().pay().build().unwrap();
    let mut bob = w.join(&w.bob, "buyer", pay).await;
    bob.app_mut().invite(&w.alice_id()).await.unwrap();
    let now = w.clock.now_ms();
    let purchase = request_from(&mut shop, w.bob_id(), draft("1", now)).await;
    bob.receive(None).await.unwrap();
    assert!(matches!(
        bob.pay_request(&purchase, PayOptions::default())
            .await
            .outcome
            .unwrap(),
        PayOutcome::Paid { .. }
    ));
    shop.receive(None).await.unwrap();
    let family = Collection::PayeeActivity;
    let credit = shop
        .app()
        .record_key(&activity_target(&purchase, family))
        .unwrap();
    let subscription = request_from(&mut shop, w.bob_id(), monthly(now, None)).await;
    bob.receive(None).await.unwrap();
    bob.accept_subscription(&subscription).await.unwrap();
    shop.receive(None).await.unwrap();
    let parent = activity_target(&subscription, family);
    let sale = shop.app().record_key(&parent).unwrap();
    shop.send_period_details(&sale, january_details(now + crate::common::HOUR_MS))
        .await
        .unwrap();
    let _next =
        recover_with_divergent_phone_writes(&w, &mut shop, &mut phone, &[(&parent, false)]).await;
    follow_loss(&mut shop).await;

    // The credit, with the user's consent that it may be credited already, is refused while
    // the sale record is divergent and not confirmed for a credit.
    let consent = || ReceiptDetails::default().with_maybe_credited_confirmed(UserConfirmed::new());
    let refused = shop
        .confirm_payment_with(&credit, consent())
        .await
        .unwrap_err();
    let Some(Refusal::Divergent { records }) = refused.refusal() else {
        panic!("not a divergent refusal: {refused:?}");
    };
    assert_eq!(
        records.iter().map(|r| r.record).collect::<Vec<_>>(),
        vec![sale]
    );
    assert!(records.iter().all(|r| r.may_hold_credit && r.divergent));
    assert_eq!(w.rail.all_executions().len(), 1);

    // The host confirms exactly the records the refusal shows, lets waiting writes go, and
    // asks again: one receipt.
    let waiting = records.iter().any(|r| r.writes_waiting);
    for shown in records.clone() {
        shop.confirm_divergent(&shown, UserConfirmed::new())
            .await
            .unwrap();
    }
    if waiting {
        shop.receive(None).await.unwrap();
    }
    let receipt = shop
        .confirm_payment_with(&credit, consent())
        .await
        .unwrap()
        .expect("the credit goes through once the user confirms the refusal's records");
    // A second confirmation sends no second receipt.
    match shop.confirm_payment_with(&credit, consent()).await.unwrap() {
        None => {}
        Some(again) => assert_eq!(again, receipt, "a second receipt was sent"),
    }
    shop.app_mut().flush_message(&receipt).await.unwrap();
    bob.receive(None).await.unwrap();
    let view = bob.request(&purchase).await.unwrap();
    assert!(view.receipt.is_some(), "{view:?}");
    assert_eq!(view.status, RequestStatus::Paid);
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// Alice's wallet, holding `paykit_pay` and `paykit_manage_contacts`, with bob's request
/// received and bob blocked; then her provider is lost, and the new service has no room for
/// bob's deny entry, so the list upload that carries it stays owed: the recovery waits at its
/// list step, while writes and receiving go on (Switch wire 7.4.14, "Lists"). Alice's phone, holding `paykit_pay` alone, follows the
/// loss too. Returns the wallet, the phone and the request.
async fn loss_with_an_owed_deny_entry(w: &World) -> (Paykit, Paykit, RequestRef) {
    loss_with_an_owed_deny_entry_after(w, false).await
}

/// [`loss_with_an_owed_deny_entry`], the wallet's first evidence upload unanswered when
/// `interrupted`, so a receive reads the lists before the list step.
async fn loss_with_an_owed_deny_entry_after(
    w: &World,
    interrupted: bool,
) -> (Paykit, Paykit, RequestRef) {
    let mut wallet = w
        .join(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new()
                .pay()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    let mut phone = w
        .join(
            &w.alice,
            "phone",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.block(&w.bob_id()).await.unwrap();
    // Each app's copy of the lists holds the block when the provider is lost: that copy is
    // what the loss's list upload carries.
    for app in [&mut wallet, &mut phone] {
        assert!(app
            .app_mut()
            .allow_list()
            .await
            .unwrap()
            .deny
            .contains(&w.bob_id()));
    }
    let next = declare_loss(w);
    if interrupted {
        next.fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/recovery/upload".into(),
            after: 0,
        });
    }
    // The new service's deny list has no room for bob's entry: each list upload is refused
    // for it (Switch wire 7.4.14, `429 quota_exceeded` with `limit` `deny`).
    for _ in 0..64 {
        next.fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/recovery/upload".into(),
            code: pubky_switch_core::http::ErrorCode::QuotaExceeded,
            current: serde_json::json!({ "limit": "deny", "over": 1 }),
        });
    }
    // Two receives when the first upload is interrupted, as few as reach the list step, so
    // nothing but the list step's own rule keeps a copy read before it from being fresh.
    let receives = if interrupted { 2 } else { 4 };
    for app in [&mut wallet, &mut phone] {
        app.app_mut().refresh_identity().await.unwrap();
        for _ in 0..receives {
            let _ = app.receive(None).await;
        }
        assert_lists_step_owes_bob(w, app).await;
    }
    (wallet, phone, request)
}

/// The recovery waits at its list step, its upload still carrying bob's deny entry.
async fn assert_lists_step_owes_bob(w: &World, app: &Paykit) {
    let info = app.app().recovery_info().await.unwrap().unwrap();
    assert_eq!(
        info.step,
        pubky_switch_client::app::RecoveryStep::Lists,
        "{info:?}"
    );
    let owed = app.app().recovery_deny_entries().await.unwrap();
    assert!(
        owed.iter().any(|entry| entry.pubky == w.bob_id()),
        "{owed:?}"
    );
}

/// Reconcile and end it as the user, as [`follow_loss`] does once the steps are done.
async fn reconcile_and_end(app: &mut Paykit) {
    let mut reconciled = app.reconcile().await;
    for _ in 0..4 {
        let Err(error) = &reconciled else { break };
        assert!(
            matches!(error.refusal(), Some(Refusal::NotNow(_))),
            "{error}"
        );
        reconciled = app.reconcile().await;
    }
    reconciled.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
}

/// The block floor `app` holds in the identity's `block_floor` record (Paykit wire 5.6).
async fn block_floor(app: &Paykit) -> Option<u64> {
    app.app()
        .records_of("block_floor")
        .await
        .unwrap()
        .iter()
        .find_map(|record| {
            let value = record.content.value()?;
            paykit_lib::records::BlockFloorRecord::read(value)
                .ok()
                .map(|record| record.floor_ms)
        })
}

#[tokio::test]
async fn test_provider_loss_owed_deny_entry_keeps_its_peer_blocked_for_payments() {
    // The new service doesn't hold bob's deny entry until the list upload lands; the lists
    // Paykit reads during the list step still count him blocked, also once the user ends the
    // reconciliation, so his request isn't paid without the user's confirmation.
    let w = World::new();
    let (mut wallet, _phone, request) = loss_with_an_owed_deny_entry(&w).await;
    reconcile_and_end(&mut wallet).await;
    // Well within the copy's refresh interval: a copy read before the list step lacks bob's
    // entry, and is not taken as fresh while the upload is owed.
    assert_lists_step_owes_bob(&w, &wallet).await;
    // Payable before the loss, the request asks first whether it was paid then.
    let doubted = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt_of(&doubted)),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Blocked { .. })),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_provider_loss_unblock_leaves_an_owed_deny_entry_out_of_the_upload() {
    // The user unblocks bob while his deny entry waits in the list upload: the unblock takes
    // him out of that upload, after raising the shared floor, rather than finding nothing to
    // remove at the new service.
    let w = World::new();
    let (mut wallet, _phone, _request) = loss_with_an_owed_deny_entry(&w).await;
    assert_eq!(block_floor(&wallet).await, None);
    wallet.unblock(&w.bob_id()).await.unwrap();
    let owed = wallet.app().recovery_deny_entries().await.unwrap();
    assert!(
        owed.iter().all(|entry| entry.pubky != w.bob_id()),
        "{owed:?}"
    );
    assert!(block_floor(&wallet).await.is_some());
}

#[tokio::test]
async fn test_provider_loss_unblock_with_lists_read_alone_raises_the_floor_first() {
    // An app holding `paykit_pay` without `paykit_manage_contacts` may still leave an owed
    // deny entry out of the upload, which unblocks its peer: it raises the shared floor
    // first, as every payer's unblock does.
    let w = World::new();
    let (_wallet, mut phone, _request) = loss_with_an_owed_deny_entry(&w).await;
    assert!(!phone
        .app()
        .cert()
        .has_permission(pubky_switch_core::scopes::Permission::ListsManage));
    assert_lists_step_owes_bob(&w, &phone).await;
    assert_eq!(block_floor(&phone).await, None);
    phone.unblock(&w.bob_id()).await.unwrap();
    let owed = phone.app().recovery_deny_entries().await.unwrap();
    assert!(
        owed.iter().all(|entry| entry.pubky != w.bob_id()),
        "{owed:?}"
    );
    assert!(block_floor(&phone).await.is_some());
}

#[tokio::test]
async fn test_provider_loss_unblock_with_lists_read_alone_keeps_a_peer_the_service_denies() {
    // The phone may only leave bob out of its own owed upload, but another own app's block
    // already holds him at the new service: he stays blocked for the phone's payments.
    let w = World::new();
    let (mut wallet, mut phone, _request) = loss_with_an_owed_deny_entry(&w).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    phone.unblock(&w.bob_id()).await.unwrap();
    let owed = phone.app().recovery_deny_entries().await.unwrap();
    assert!(
        owed.iter().all(|entry| entry.pubky != w.bob_id()),
        "{owed:?}"
    );
    assert!(phone
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    // The phone's copy of the deny list, which its payments are judged by, still holds him.
    let copy: serde_json::Value = serde_json::from_slice(
        &phone
            .app()
            .protocol_get("paykit", b"dl")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let copy: Vec<[u8; 32]> = serde_json::from_value(copy["d"].clone()).unwrap();
    assert!(copy.contains(&w.bob_id()));
}

#[tokio::test]
async fn test_provider_loss_list_read_before_the_list_step_isnt_taken_as_fresh() {
    // The evidence upload is interrupted once, and a receive reads the lists meanwhile, when
    // the new service holds none of the loss's deny entries and the client owes them all.
    // At the list step, seconds later, that copy isn't taken as fresh: bob stays blocked for
    // payments (Switch wire 7.4.14, "Lists"; Paykit spec 8.1).
    let w = World::new();
    let (mut wallet, _phone, request) = loss_with_an_owed_deny_entry_after(&w, true).await;
    reconcile_and_end(&mut wallet).await;
    let doubted = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt_of(&doubted)),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Blocked { .. })),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

/// Alice's app, holding `scopes`, with bob blocked and the lists read; then her provider is
/// lost, and the app's first evidence upload goes unanswered, so a receive runs while the
/// recovery is at its upload step, and a second follows (Switch spec 12.6). When `stays`,
/// every later upload goes unanswered too, and the recovery waits at that step.
async fn loss_interrupted_at_upload(
    w: &World,
    scopes: paykit_sdk::AppScopes,
    stays: bool,
) -> (Paykit, Option<RequestRef>) {
    let mut app = w.join(&w.alice, "app", scopes.build().unwrap()).await;
    app.app_mut().invite(&w.bob_id()).await.unwrap();
    let request = if app.app().cert().scopes().iter().any(|s| s == "paykit_pay") {
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        app.receive(None).await.unwrap();
        Some(request)
    } else {
        None
    };
    app.block(&w.bob_id()).await.unwrap();
    assert!(app
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    app.receive(None).await.unwrap();
    let next = declare_loss(w);
    for _ in 0..if stays { 64 } else { 1 } {
        next.fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/recovery/upload".into(),
            after: 0,
        });
    }
    app.app_mut().refresh_identity().await.unwrap();
    for _ in 0..2 {
        let _ = app.receive(None).await;
    }
    (app, request)
}

/// The recovery step `app` is at.
async fn recovery_step(app: &Paykit) -> pubky_switch_client::app::RecoveryStep {
    app.app().recovery_info().await.unwrap().unwrap().step
}

/// The deny list `app` keeps as its copy (`dl`), and whether it holds a read time (`lr`).
async fn deny_copy(app: &Paykit) -> (Vec<[u8; 32]>, bool) {
    let copy: serde_json::Value = serde_json::from_slice(
        &app.app()
            .protocol_get("paykit", b"dl")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let read = app.app().protocol_get("paykit", b"lr").await.unwrap();
    (
        serde_json::from_value(copy["d"].clone()).unwrap(),
        read.is_some(),
    )
}

#[tokio::test]
async fn test_provider_loss_list_read_at_the_upload_step_keeps_the_stored_deny_list() {
    // At the upload step the new service holds none of the loss's deny entries, and the
    // client names none yet: a payer's or a seller's read of the lists then takes nothing,
    // keeps bob in its copy, raises no floor, and holds no copy fresh (Switch wire 7.4.14).
    use paykit_sdk::AppScopes;
    for scopes in [
        AppScopes::new().pay().manage_contacts(),
        AppScopes::new().request().manage_contacts(),
    ] {
        let w = World::new();
        let (app, _) = loss_interrupted_at_upload(&w, scopes, true).await;
        assert_eq!(
            recovery_step(&app).await,
            pubky_switch_client::app::RecoveryStep::Upload
        );
        let (copy, fresh) = deny_copy(&app).await;
        assert!(copy.contains(&w.bob_id()), "{copy:?}");
        assert!(!fresh);
        assert_eq!(block_floor(&app).await, None);
    }
}

#[tokio::test]
async fn test_provider_loss_upload_finished_in_a_receive_keeps_its_peer_blocked() {
    // The receive that reads the lists at the upload step finishes the recovery's steps: no
    // copy read before them is taken as fresh, and bob stays blocked for payments.
    use paykit_sdk::AppScopes;
    let w = World::new();
    let (mut wallet, request) =
        loss_interrupted_at_upload(&w, AppScopes::new().pay().manage_contacts(), false).await;
    let request = request.unwrap();
    assert!(
        recovery_step(&wallet).await > pubky_switch_client::app::RecoveryStep::Lists,
        "{:?}",
        recovery_step(&wallet).await
    );
    reconcile_and_end(&mut wallet).await;
    let doubted = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt_of(&doubted)),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Blocked { .. })),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(block_floor(&wallet).await, None);
}

#[tokio::test]
async fn test_provider_loss_send_to_a_blocked_peer_waits_after_a_read_at_the_upload_step() {
    // A seller's read of the lists at the upload step, in the receive that then finishes the
    // recovery's steps, takes nothing: a payment request to bob, blocked, isn't sent on a
    // copy read before them (Switch wire 7.4.14; Paykit spec 8.1).
    use paykit_sdk::AppScopes;
    let w = World::new();
    // Bob's switch admits alice: only alice's block of him can stop the send.
    let mut buyer = w
        .join(&w.bob, "buyer", AppScopes::new().pay().build().unwrap())
        .await;
    buyer.app_mut().invite(&w.alice_id()).await.unwrap();
    let (mut shop, _) =
        loss_interrupted_at_upload(&w, AppScopes::new().request().manage_contacts(), false).await;
    assert!(recovery_step(&shop).await > pubky_switch_client::app::RecoveryStep::Lists);
    let entries = w.bob.switch().entries_from(&w.alice_id());
    let _ = shop
        .send_request(w.bob_id(), draft("2", w.clock.now_ms()))
        .await;
    let _ = shop.flush().await;
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
}

#[tokio::test]
async fn test_provider_loss_unblock_at_the_upload_step_waits_for_the_list_step() {
    // Before the list step nothing tells an unblock from a removal of nothing: it is refused
    // for now, with nothing changed, and goes once the step comes.
    use paykit_sdk::AppScopes;
    let w = World::new();
    let (mut wallet, _) =
        loss_interrupted_at_upload(&w, AppScopes::new().pay().manage_contacts(), true).await;
    let refused = wallet.unblock(&w.bob_id()).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(paykit_sdk::NotNowReason::ReadAgain(why))) if why.contains("after the next receive")),
        "{refused:?}"
    );
    let (copy, _) = deny_copy(&wallet).await;
    assert!(copy.contains(&w.bob_id()));
    assert_eq!(block_floor(&wallet).await, None);
}

#[tokio::test]
async fn test_provider_loss_list_read_at_the_list_step_is_fresh_and_holds_the_owed_entry() {
    // At the list step a read of the lists holds the loss's owed deny entries, and is right
    // once they land: the copy is fresh, so a deny list that stays full holds nothing else
    // back (Switch wire 7.4.14, "Lists").
    let w = World::new();
    let (mut wallet, _phone, _request) = loss_with_an_owed_deny_entry(&w).await;
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    let _ = wallet.receive(None).await;
    assert_lists_step_owes_bob(&w, &wallet).await;
    let (copy, fresh) = deny_copy(&wallet).await;
    assert!(copy.contains(&w.bob_id()), "{copy:?}");
    assert!(fresh);
}

#[tokio::test]
async fn test_provider_loss_followed_through_the_client_reads_the_lists_again() {
    // The host follows the loss through the client alone, every step done before Paykit's
    // next read: the read time from before the loss is gone with it, so no copy read before
    // the loss is taken as fresh.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_some());
    let _next = declare_loss(&w);
    wallet.app_mut().refresh_identity().await.unwrap();
    for _ in 0..4 {
        let _ = wallet.app_mut().follow_provider().await;
    }
    assert!(wallet
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_provider_loss_upload_goes_on_with_no_deny_list_copy() {
    // An app that keeps no copy of the deny list, its recovery stopped at the evidence
    // upload: its reads of the lists leave the copy unknown rather than stop the receive, so
    // the next receive uploads once the service answers, and the recovery goes on.
    use pubky_switch_client::store::WriteBatch;
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    wallet.receive(None).await.unwrap();
    let next = declare_loss(&w);
    next.fail_next(pubky_switch_client::testing::Fault::Unreachable {
        path: "/recovery/upload".into(),
        after: 0,
    });
    wallet.app_mut().refresh_identity().await.unwrap();
    let _ = wallet.receive(None).await;
    assert_eq!(
        recovery_step(&wallet).await,
        pubky_switch_client::app::RecoveryStep::Upload
    );
    // Paykit's copy of the deny list goes, as for an app that never kept one.
    let ns = pubky_switch_client::store::client_namespace(&wallet.app_id());
    let copy = base
        .scan(&ns, b"p/paykit", None, 10_000)
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .find(|key| key.len() == b"p/paykit".len() + 3 && key.ends_with(b"dl"))
        .unwrap();
    let mut batch = WriteBatch::new();
    batch.delete(&ns, copy).unwrap();
    base.commit(batch).unwrap();
    assert!(wallet
        .app()
        .protocol_get("paykit", b"dl")
        .await
        .unwrap()
        .is_none());
    wallet.receive(None).await.unwrap();
    assert!(recovery_step(&wallet).await > pubky_switch_client::app::RecoveryStep::Upload);
}

#[tokio::test]
async fn test_provider_loss_read_before_the_loss_isnt_fresh_at_the_list_step() {
    // The shop's lists were read before the loss, minutes before; then its client blocked
    // carol, which the shop's copy doesn't hold yet. The first receive after the loss stops
    // at the list step, carol's entry not fitting the new service's deny list. That read from
    // before the loss doesn't count as fresh: a request to carol isn't sent (Switch wire
    // 7.4.14, "Lists"; Paykit spec 8.1).
    use paykit_sdk::AppScopes;
    let w = World::new();
    let carol_id = w.carol.identity().public();
    let mut buyer = w
        .join(&w.carol, "buyer", AppScopes::new().pay().build().unwrap())
        .await;
    buyer.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut shop = w
        .join(
            &w.alice,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    shop.receive(None).await.unwrap();
    shop.app_mut().deny(&carol_id).await.unwrap();
    assert!(shop
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&carol_id));
    let next = declare_loss(&w);
    for _ in 0..64 {
        next.fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/recovery/upload".into(),
            code: pubky_switch_core::http::ErrorCode::QuotaExceeded,
            current: serde_json::json!({ "limit": "deny", "over": 1 }),
        });
    }
    shop.app_mut().refresh_identity().await.unwrap();
    let _ = shop.receive(None).await;
    assert_eq!(
        recovery_step(&shop).await,
        pubky_switch_client::app::RecoveryStep::Lists
    );
    let entries = w.carol.switch().entries_from(&w.alice_id());
    let _ = shop
        .send_request(carol_id, draft("1", w.clock.now_ms()))
        .await;
    let _ = shop.flush().await;
    assert_eq!(w.carol.switch().entries_from(&w.alice_id()), entries);
}
