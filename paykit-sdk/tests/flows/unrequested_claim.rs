//! The claim of a payment without a request (Paykit spec 7.1, 7.3, 8.4; Paykit wire 1.5):
//! derived from the attempt ID blinded with the payer's naming key, so every own app finds
//! the same claim, a lost answer, a restore and a late receipt all use the original claim
//! and attempt, and a claim at the attempt ID's unblinded derivation is never read or taken
//! as the payment's.

use std::sync::Arc;

use paykit_lib::derive::payment_obligation;
use paykit_lib::AttemptId;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    PayOptions, PayOutcome, Paykit, PaykitConfig, Rail, RailDeclaration, Resolution, Stage,
    UnrequestedPayment,
};
use pubky_dispatch_client::app::UserConfirmed;
use pubky_dispatch_client::testing::Fault;
use pubky_dispatch_core::claims::{ClaimStatus, Outcome};

use crate::common::{btc, endpoint, list_version, store, World, HOUR_MS};

/// An online payment without a request: to the payee's Private Payment List, or to its
/// public endpoint (Paykit spec 8.4, Destinations).
#[derive(Clone, Copy, Debug)]
enum Mode {
    List,
    Public,
}

/// Bob's shop, offering alice's `wallet` what `mode` pays: a list shared with the wallet, or
/// a public endpoint on his Homeserver.
async fn shop_for(w: &World, mode: Mode, wallet: &mut Paykit) -> Paykit {
    let mut shop = match mode {
        Mode::List => w.shop().await,
        Mode::Public => crate::offline_public::shop_with_endpoint(w).await,
    };
    if matches!(mode, Mode::List) {
        w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
            .await;
    }
    wallet.receive(None).await.unwrap();
    shop
}

/// The payment `mode` makes to bob's reusable `btc-lightning-bolt12` endpoint.
fn to_bob(w: &World, mode: Mode) -> UnrequestedPayment {
    match mode {
        Mode::List => UnrequestedPayment::new(w.bob_id(), btc("0.0015"))
            .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12"))),
        Mode::Public => crate::offline_public::payment(w),
    }
}

/// The claim key of `attempt` as the payer's app `app` derives it (Paykit wire 1.5).
fn claim_key(app: &Paykit, attempt: &AttemptId) -> [u8; 32] {
    payment_obligation(app.app().naming_key(), attempt)
}

/// `derive("paykit/v1/obligation", "payment", attempt ID)`, the attempt ID not blinded: no
/// payment's claim key (Paykit wire 1.5).
fn unblinded(attempt: &AttemptId) -> [u8; 32] {
    pubky_dispatch_core::enc::derive("paykit/v1/obligation", &[b"payment", &attempt.to_bytes()])
}

/// Whether any request the router logged from `from` on names the claim `key`.
fn read_of(w: &World, from: usize, key: &[u8; 32]) -> bool {
    let key = pubky_dispatch_core::enc::b64(key);
    w.router.log()[from..]
        .iter()
        .any(|line| line.contains(&key))
}

/// The claim key of the wallet's only journal entry, which pays `attempt`: the key
/// [`claim_key`] derives for this payer.
async fn journaled_on(wallet: &Paykit, attempt: &AttemptId) -> [u8; 32] {
    let payments = wallet.payments().await.unwrap();
    assert_eq!(payments.len(), 1, "{payments:?}");
    assert_eq!(payments[0].attempt_id, *attempt);
    assert_eq!(payments[0].obligation, claim_key(wallet, attempt));
    payments[0].obligation
}

/// The claim at `key` holds `attempt` alone, with `outcome`.
async fn only_attempt(app: &mut Paykit, key: &[u8; 32], attempt: &AttemptId, outcome: Outcome) {
    let view = app.app_mut().read_claim(key).await.unwrap().unwrap().view;
    assert_eq!(view.attempts.len(), 1, "{view:?}");
    assert_eq!(
        view.attempt(&attempt.to_bytes()).unwrap().outcome,
        outcome,
        "{view:?}"
    );
}

/// A rail without a history and idempotent for a day: a resume decides a handed off payment
/// by what its claim shows (Paykit spec 7.2, "What a payment method declares").
fn idempotent_without_history() -> RailDeclaration {
    RailDeclaration::new(
        paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
        paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
    )
    .with_idempotent(true)
    .with_idempotency_retention_ms(24 * HOUR_MS)
}

async fn lost_reply(mode: Mode) {
    // The answer to the batch writing the attempt and its record is lost after the service
    // applied it: no money moved, and a resume writes `failed` on that attempt. The next
    // payment's method call goes through but its answer is lost: a resume finds it paid in
    // the method's history and writes `done` on the original claim, with no second call
    // (Paykit spec 7.2, 8.4).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = shop_for(&w, mode, &mut wallet).await;
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let unsent = wallet.pay_without_request(to_bob(&w, mode)).await.outcome;
    assert!(!matches!(unsent, Ok(PayOutcome::Paid { .. })), "{unsent:?}");
    let unpaid = wallet.payments().await.unwrap()[0].attempt_id;
    let unpaid_key = journaled_on(&wallet, &unpaid).await;
    wallet.resume_payments().await.unwrap();
    only_attempt(&mut wallet, &unpaid_key, &unpaid, Outcome::Failed).await;
    assert_eq!(w.rail.pay_calls(&unpaid), 0);

    w.rail.next(Behaviour::LoseAnswer);
    let lost = wallet.pay_without_request(to_bob(&w, mode)).await.outcome;
    assert!(!matches!(lost, Ok(PayOutcome::Paid { .. })), "{lost:?}");
    let attempt = wallet
        .payments()
        .await
        .unwrap()
        .into_iter()
        .map(|payment| payment.attempt_id)
        .find(|id| *id != unpaid)
        .unwrap();
    let key = claim_key(&wallet, &attempt);
    assert_ne!(key, unpaid_key);
    only_attempt(&mut wallet, &key, &attempt, Outcome::Open).await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        resumed.iter().any(
            |outcome| matches!(outcome, PayOutcome::Paid { attempt_id: id, .. } if *id == attempt)
        ),
        "{resumed:?}"
    );
    wallet.resume_payments().await.unwrap();
    only_attempt(&mut wallet, &key, &attempt, Outcome::Done).await;
    let payments = wallet.payments().await.unwrap();
    let paid = payments.iter().find(|p| p.attempt_id == attempt).unwrap();
    assert_eq!(paid.obligation, key);
    assert_eq!(paid.stage, Stage::Finished);
    assert_eq!(w.rail.pay_calls(&attempt), 1);
    assert_eq!(w.rail.all_pay_calls().len(), 1);
}

#[tokio::test]
async fn test_unrequested_claim_lost_reply_to_a_list_uses_the_original_claim() {
    lost_reply(Mode::List).await;
}

#[tokio::test]
async fn test_unrequested_claim_lost_reply_to_a_public_endpoint_uses_the_original_claim() {
    lost_reply(Mode::Public).await;
}

async fn restored(mode: Mode) {
    // A backup taken with the payment in flight; the method pays it, and the app restored
    // from the backup finishes it on the claim it derives, with no second payment call.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = shop_for(&w, mode, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    let outcome = wallet
        .pay_without_request(to_bob(&w, mode))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    let key = journaled_on(&wallet, &attempt_id).await;
    let backup = wallet.backup().await.unwrap();
    w.rail.settle(&attempt_id, true);
    drop(wallet);
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    app.receive(None).await.unwrap();
    let report = app.reconcile().await.unwrap();
    assert!(
        matches!(report.resolved.as_slice(), [PayOutcome::Paid { attempt_id: id, .. }] if *id == attempt_id),
        "{report:?}"
    );
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    assert_eq!(journaled_on(&app, &attempt_id).await, key);
    only_attempt(&mut app, &key, &attempt_id, Outcome::Done).await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_unrequested_claim_restored_payment_to_a_list_finishes_on_the_original_claim() {
    restored(Mode::List).await;
}

#[tokio::test]
async fn test_unrequested_claim_restored_payment_to_a_public_endpoint_finishes_on_the_original_claim(
) {
    restored(Mode::Public).await;
}

/// `sender`'s receipt for alice's `wallet` of `attempt`, naming the proof `proof`.
fn receipt_from(
    w: &World,
    sender: &pubky_dispatch_client::testing::TestSigner,
    wallet: &Paykit,
    attempt: AttemptId,
    proof: [u8; 32],
) {
    let body = paykit_lib::bodies::PaymentReceipt {
        attempt_id: attempt,
        billing_period: None,
        metadata: None,
        payment_reference: None,
    }
    .to_body(1)
    .unwrap();
    w.inject(
        sender,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_receipt",
        vec![proof],
        body,
    );
}

async fn late_receipt(mode: Mode) {
    // The method reports the payment failed, and the wallet writes `failed`; it completed
    // after all, and the proof goes out late. Receipts from a third party, or naming another
    // attempt, leave the claim unresolved; the payee's, past the claim's deadline, settles
    // the original attempt on the original claim (Paykit spec 7.3, 8.4 "After paying").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_for(&w, mode, &mut wallet).await;
    let _carol = w
        .join(
            &w.carol,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    w.rail.next(Behaviour::Fail);
    let outcome = wallet
        .pay_without_request(to_bob(&w, mode))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Failed { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    let key = journaled_on(&wallet, &attempt_id).await;
    w.rail.complete_after_failure(&attempt_id);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [PayOutcome::Paid {
                proof_msg_id: Some(_),
                ..
            }]
        ),
        "{resumed:?}"
    );
    only_attempt(&mut wallet, &key, &attempt_id, Outcome::Failed).await;
    let proof = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|sent| sent.kind == "paykit.payment_proof")
        .unwrap()
        .plaintext
        .hash();
    w.clock.advance_ms(HOUR_MS);
    receipt_from(&w, &w.carol, &wallet, attempt_id, proof);
    receipt_from(
        &w,
        &w.bob,
        &wallet,
        AttemptId::from_bytes([0x5a; 16]),
        proof,
    );
    wallet.receive(None).await.unwrap();
    let view = wallet
        .app_mut()
        .read_claim(&key)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_ne!(view.status, ClaimStatus::Settled, "{view:?}");
    only_attempt(&mut wallet, &key, &attempt_id, Outcome::Failed).await;
    assert!(wallet.payments().await.unwrap()[0].receipt.is_none());
    // The payee's own receipt settles it.
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].attempt_id, Some(attempt_id));
    assert!(shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    wallet.receive(None).await.unwrap();
    let view = wallet
        .app_mut()
        .read_claim(&key)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(view.status, ClaimStatus::Settled, "{view:?}");
    only_attempt(&mut wallet, &key, &attempt_id, Outcome::Done).await;
    assert!(wallet.payments().await.unwrap()[0].receipt.is_some());
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_unrequested_claim_late_receipt_for_a_list_payment_settles_the_original_claim() {
    late_receipt(Mode::List).await;
}

#[tokio::test]
async fn test_unrequested_claim_late_receipt_for_a_public_payment_settles_the_original_claim() {
    late_receipt(Mode::Public).await;
}

async fn second_wallet(mode: Mode) {
    // The wallet's payment is in flight. Alice's phone, joined later, finds its claim from
    // the payment's `payer_activity` record: the same key, held by its live owner, which
    // the phone can't resolve until the attempt is orphaned (Paykit spec 7.1, 7.3).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = shop_for(&w, mode, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    let outcome = wallet
        .pay_without_request(to_bob(&w, mode))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    let key = journaled_on(&wallet, &attempt_id).await;
    let mut phone = w.wallet("phone").await;
    assert_eq!(claim_key(&phone, &attempt_id), key);
    let report = phone.reconcile().await.unwrap();
    let doubtful = report
        .doubtful
        .iter()
        .find(|d| d.obligation == key)
        .unwrap_or_else(|| panic!("{report:?}"));
    assert_eq!(doubtful.attempts, [attempt_id]);
    assert_eq!(doubtful.payee, Some(w.bob_id()));
    assert_eq!(doubtful.request_hash, None);
    let view = phone
        .app_mut()
        .read_claim(&key)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(view.owner, Some(wallet.app_id()));
    // The owner is current and the attempt not orphaned: the phone's `done` is refused.
    assert!(phone
        .resolve_attempt(&key, attempt_id, Resolution::Done, UserConfirmed::new())
        .await
        .is_err());
    only_attempt(&mut phone, &key, &attempt_id, Outcome::Open).await;
    // Orphaned, the user resolves it from the phone as paid.
    w.clock.advance_ms(2 * HOUR_MS);
    phone
        .resolve_attempt(&key, attempt_id, Resolution::Done, UserConfirmed::new())
        .await
        .unwrap();
    let view = phone
        .app_mut()
        .read_claim(&key)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(view.status, ClaimStatus::Settled, "{view:?}");
    only_attempt(&mut phone, &key, &attempt_id, Outcome::Done).await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_unrequested_claim_second_wallet_finds_a_list_payments_claim_from_its_record() {
    second_wallet(Mode::List).await;
}

#[tokio::test]
async fn test_unrequested_claim_second_wallet_finds_a_public_payments_claim_from_its_record() {
    second_wallet(Mode::Public).await;
}

async fn unblinded_claim_after_loss(mode: Mode) {
    // The payment is in flight when the device and alice's service are both lost; the app
    // restored from a backup holding the handed off entry follows the loss, and the new
    // service holds no claim for it. A claim holding the attempt, open and the app's own,
    // sits only at the attempt ID's unblinded derivation. The restored app never reads it,
    // writes to it or takes it as the payment's: it finds nothing at its own claim key, takes
    // nothing as paid and pays nothing again (Paykit spec 7.2, Paykit wire 1.5).
    let w = World::new();
    w.rail.set_declaration(idempotent_without_history());
    let mut wallet = w.wallet("wallet").await;
    let _shop = shop_for(&w, mode, &mut wallet).await;
    w.rail.next(Behaviour::Hang);
    let outcome = wallet
        .pay_without_request(to_bob(&w, mode))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    let key = journaled_on(&wallet, &attempt_id).await;
    let unblinded = unblinded(&attempt_id);
    assert_ne!(key, unblinded);
    let app_id = wallet.app_id();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    w.clock.advance_ms(60_000);
    let next = crate::provider_loss::declare_loss(&w);
    next.open_attempt(unblinded, attempt_id.to_bytes(), app_id)
        .unwrap();
    let from = w.router.log().len();
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(app.app_id(), app_id);
    app.receive(None).await.unwrap();
    crate::provider_loss::follow_loss(&mut app).await;
    app.resume_payments().await.unwrap();
    app.reconcile().await.unwrap();
    app.resume_payments().await.unwrap();
    assert!(read_of(&w, from, &key));
    assert!(!read_of(&w, from, &unblinded));
    assert!(app.app_mut().read_claim(&key).await.unwrap().is_none());
    let payments = app.payments().await.unwrap();
    assert_eq!(payments.len(), 1, "{payments:?}");
    assert_eq!(payments[0].obligation, key);
    assert!(
        !matches!(payments[0].stage, Stage::Finished | Stage::RailPaid),
        "{payments:?}"
    );
    let planted = next.claim(&unblinded).unwrap();
    assert_eq!(planted.attempts.len(), 1);
    assert_eq!(planted.attempts[0].outcome, Outcome::Open);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_unrequested_claim_list_payment_never_takes_the_unblinded_claim() {
    unblinded_claim_after_loss(Mode::List).await;
}

#[tokio::test]
async fn test_unrequested_claim_public_payment_never_takes_the_unblinded_claim() {
    unblinded_claim_after_loss(Mode::Public).await;
}

#[tokio::test]
async fn test_unrequested_claim_offline_public_payment_never_takes_the_unblinded_claim() {
    // Paid while alice's Dispatch was unreachable, so no claim was written, then restored from
    // a backup holding the paid journal entry. A claim holding the attempt sits only at the
    // attempt ID's unblinded derivation, open and the app's. The restored app finds nothing
    // at its own claim key and keeps the payment unrecorded, waiting on that claim, without
    // reading the other key, writing to it or paying again (Paykit spec 8.4, "Without the
    // Dispatch"; Paykit wire 1.5).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = crate::offline_public::shop_with_endpoint(&w).await;
    wallet.receive(None).await.unwrap();
    w.router.remove(w.alice.origin());
    let attempt_id = wallet
        .pay_without_request(crate::offline_public::payment(&w))
        .await
        .outcome
        .unwrap()
        .attempt_id();
    let key = journaled_on(&wallet, &attempt_id).await;
    let unblinded = unblinded(&attempt_id);
    let app_id = wallet.app_id();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    w.router.add_switch(w.alice.origin(), w.alice.switch());
    w.alice
        .switch()
        .open_attempt(unblinded, attempt_id.to_bytes(), app_id)
        .unwrap();
    let from = w.router.log().len();
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.app_id(), app_id);
    restored.receive(None).await.unwrap();
    restored.reconcile().await.unwrap();
    restored.end_reconcile(UserConfirmed::new()).await.unwrap();
    restored.resume_payments().await.unwrap();
    restored.resume_payments().await.unwrap();
    assert!(read_of(&w, from, &key));
    assert!(!read_of(&w, from, &unblinded));
    assert_eq!(journaled_on(&restored, &attempt_id).await, key);
    assert_eq!(restored.payments().await.unwrap()[0].stage, Stage::RailPaid);
    assert!(restored.app_mut().read_claim(&key).await.unwrap().is_none());
    let planted = w.alice.switch().claim(&unblinded).unwrap();
    assert_eq!(planted.attempts.len(), 1);
    assert_eq!(planted.attempts[0].outcome, Outcome::Open);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

async fn journal_on_the_unblinded_claim_refused(mode: Mode) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = shop_for(&w, mode, &mut wallet).await;
    let paid = wallet.pay_without_request(to_bob(&w, mode)).await.outcome;
    let Ok(PayOutcome::Paid { attempt_id, .. }) = paid else {
        panic!("{paid:?}");
    };
    let good: serde_json::Value =
        serde_json::from_slice(&wallet.backup().await.unwrap().to_bytes().unwrap()).unwrap();
    let mut state = crate::restore::layer_of(&good);
    let journal = state
        .iter()
        .position(|(k, _)| k.starts_with(b"j/"))
        .expect("the payment is journaled");
    assert_eq!(
        state[journal].1["d"]["obligation"],
        serde_json::Value::String(pubky_dispatch_core::enc::b64(&claim_key(
            &wallet,
            &attempt_id
        )))
    );
    // An entry on the attempt's unblinded derivation is a corrupt store: refused, not taken
    // or migrated (Paykit wire 1.5).
    state[journal].1["d"]["obligation"] =
        serde_json::Value::String(pubky_dispatch_core::enc::b64(&unblinded(&attempt_id)));
    let target = store();
    crate::restore::refused_restore(
        &w,
        crate::restore::with_layer(&good, state),
        Arc::clone(&target),
    )
    .await;
    crate::restore::nothing_restored(&w, target, wallet.app_id()).await;
}

#[tokio::test]
async fn test_unrequested_claim_journal_of_a_list_payment_on_the_unblinded_claim_refused() {
    journal_on_the_unblinded_claim_refused(Mode::List).await;
}

#[tokio::test]
async fn test_unrequested_claim_journal_of_a_public_payment_on_the_unblinded_claim_refused() {
    journal_on_the_unblinded_claim_refused(Mode::Public).await;
}
