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
    assert_eq!(notices[0].attempt_id, attempt_id);
    assert_eq!(notices[0].payee, w.bob_id());
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
