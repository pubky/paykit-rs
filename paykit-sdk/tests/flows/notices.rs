//! Notices for the host (Paykit spec 7.2): "marked failed, was paid" for a receipt that
//! arrives once the claim of a payment written `failed` is gone, listed until acknowledged
//! and kept in backups.

use std::sync::Arc;

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{NoticeKind, PayOptions, PayOutcome, Paykit, PaykitConfig, Rail};
use pubky_switch_client::clock::Clock;
use pubky_switch_core::claims::ClaimStatus;
use pubky_switch_core::records::DAY_MS;

use crate::common::{draft, request_from, store, World};

/// Past a request's deadline, a day after it was sent, and the 30 days its claim is kept.
const PAST_THE_CLAIM_MS: u64 = 32 * DAY_MS;

/// Alice's wallet pays bob's request; the method reports a failure, `failed` is written,
/// and its history then shows the payment made, so the proof goes (Paykit spec 7.2). Bob
/// takes the proof. Returns the wallet, the shop and the attempt.
async fn paid_after_failed(w: &World) -> (Paykit, Paykit, paykit_lib::AttemptId) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Fail);
    let PayOutcome::Failed { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("failed");
    };
    w.rail.complete_after_failure(&attempt_id);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    shop.receive(None).await.unwrap();
    (wallet, shop, attempt_id)
}

/// Bob confirms the payment he holds a proof of: his receipt goes to alice.
async fn receipt(shop: &mut Paykit) {
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
}

#[tokio::test]
async fn test_notices_late_receipt_of_a_payment_written_failed_raises_one() {
    let w = World::new();
    let (mut wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    // The journal keeps the payment past its claim's window, for its receipt.
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    receipt(&mut shop).await;
    wallet.receive(None).await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, NoticeKind::PaidAfterFailed);
    assert_eq!(notices[0].attempt_id, Some(attempt_id));
    assert_eq!(notices[0].counterparty, w.bob_id());
    // The receipt is in the activity record.
    let key = wallet.payments().await.unwrap()[0].request_hash.unwrap();
    let view = wallet
        .request(&paykit_sdk::RequestRef {
            payee: w.bob_id(),
            request_hash: key,
        })
        .await
        .unwrap();
    let held = wallet
        .app()
        .shared_record(&view.record)
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(!value["receipt"].is_null());
    // A backup carries it, unacknowledged.
    let backup = wallet.backup().await.unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.notices().await.unwrap(), notices);
    drop(restored);
    // Acknowledged, it is no longer listed.
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
    assert!(wallet.acknowledge_notice("not-a-notice").await.is_err());
    // Its receipt came: the entry goes past its window, its acknowledged notice with it.
    wallet.resume_payments().await.unwrap();
    assert!(wallet.payments().await.unwrap().is_empty());
    let kept = wallet.app().protocol_scan(b"nt/").await.unwrap();
    assert!(kept.is_empty());
}

#[tokio::test]
async fn test_notices_receipt_within_the_claim_settles_it_and_raises_none() {
    let w = World::new();
    let (mut wallet, mut shop, _) = paid_after_failed(&w).await;
    receipt(&mut shop).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let claim = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Settled);
}

#[tokio::test]
async fn test_notices_late_receipt_of_a_payment_written_done_raises_none() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    receipt(&mut shop).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_notices_receipt_within_the_claim_lets_the_payment_be_pruned() {
    let w = World::new();
    let (mut wallet, mut shop, _) = paid_after_failed(&w).await;
    receipt(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // The receipt settled the claim: past the claim's window the entry goes, waiting for
    // no receipt.
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_notices_acknowledged_once_its_payment_is_pruned_goes() {
    let w = World::new();
    let (mut wallet, mut shop, _) = paid_after_failed(&w).await;
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    receipt(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // The entry goes first; its notice stays until the host acknowledges it, then goes.
    wallet.resume_payments().await.unwrap();
    assert!(wallet.payments().await.unwrap().is_empty());
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1);
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    assert!(wallet.app().protocol_scan(b"nt/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_notices_late_receipt_raised_by_an_own_app_without_the_payment() {
    let w = World::new();
    // Alice's second wallet holds the payer's record, not the payment's journal entry.
    let mut other = w.wallet("second").await;
    let (_wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    other.receive(None).await.unwrap();
    assert!(other.payments().await.unwrap().is_empty());
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    receipt(&mut shop).await;
    other.receive(None).await.unwrap();
    let notices = other.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, NoticeKind::PaidAfterFailed);
    assert_eq!(notices[0].attempt_id, Some(attempt_id));
}

#[tokio::test]
async fn test_notices_second_receipt_after_the_window_raises_none_where_one_is_held() {
    let w = World::new();
    let mut other = w.wallet("second").await;
    let (_wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    other.receive(None).await.unwrap();
    receipt(&mut shop).await;
    other.receive(None).await.unwrap();
    // Past the window, another receipt of the same proof: the record holds one already.
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    let view = other.requests().await.unwrap().remove(0);
    let held = other
        .app()
        .shared_record(&view.record)
        .await
        .unwrap()
        .unwrap();
    let record = paykit_lib::records::ActivityRecord::read(
        held.content.value().unwrap(),
        &w.alice_id(),
        paykit_lib::records::ActivitySide::Payer,
    )
    .unwrap();
    let proof = record.proof.unwrap().hash();
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
        &[other.app_id()],
        "paykit.payment_receipt",
        vec![proof, view.request.request_hash],
        body,
    );
    other.receive(None).await.unwrap();
    assert!(other.notices().await.unwrap().is_empty());
}
