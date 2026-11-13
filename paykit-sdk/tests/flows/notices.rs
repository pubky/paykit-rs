//! Notices for the host (Paykit spec 7.2): "marked failed, was paid" for a receipt that
//! arrives once the claim of a payment written `failed` is gone, listed until acknowledged
//! and kept in backups.

use std::sync::Arc;

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{NoticeKind, PayOptions, PayOutcome, Paykit, PaykitConfig, Rail};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::Fault;
use pubky_switch_core::claims::ClaimStatus;
use pubky_switch_core::http::ErrorCode;
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

/// The next `n` batches at alice's switch are refused for good, `403 permission_missing`.
fn refuse_writes(w: &World, n: usize) {
    for _ in 0..n {
        w.alice.switch().fail_next(Fault::Error {
            path: "/ops".into(),
            code: ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    }
}

/// Whether the wallet's journal entry of `attempt_id` waits for a late receipt.
async fn waits_for_receipt(wallet: &Paykit, attempt_id: paykit_lib::AttemptId) -> bool {
    let key = format!("j/{}", attempt_id.to_text());
    let bytes = wallet
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let entry: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    entry["d"]["failed_on_claim"] == serde_json::Value::Bool(true)
}

#[tokio::test]
async fn test_notices_late_receipt_written_later_raises_one_and_ends_the_wait() {
    let w = World::new();
    let (mut wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    receipt(&mut shop).await;
    // The receipt's step is refused for good, and so is its first later write: its entry
    // is stored, and the receipt is written later, with the same effects as at its first
    // read.
    refuse_writes(&w, 2);
    wallet.receive(None).await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
    assert!(waits_for_receipt(&wallet, attempt_id).await);
    wallet.receive(None).await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, NoticeKind::PaidAfterFailed);
    assert_eq!(notices[0].attempt_id, Some(attempt_id));
    assert_eq!(notices[0].counterparty, w.bob_id());
    assert!(!waits_for_receipt(&wallet, attempt_id).await);
    assert!(wallet.payments().await.unwrap()[0].receipt.is_some());
    // Raised once, and the entry goes past its window like any other.
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.notices().await.unwrap(), notices);
    wallet.resume_payments().await.unwrap();
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_notices_late_receipt_raised_by_an_owner_restored_from_before_the_failure() {
    // The wallet's backup predates the payment: restored, it has no journal entry of the
    // attempt, and holds the failure from the claim view its reconcile reads.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    w.rail.next(Behaviour::Fail);
    let PayOutcome::Failed { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("failed");
    };
    w.rail.complete_after_failure(&attempt_id);
    wallet.resume_payments().await.unwrap();
    shop.receive(None).await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    restored.reconcile().await.unwrap();
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    assert!(restored.payments().await.unwrap().is_empty());
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    receipt(&mut shop).await;
    restored.receive(None).await.unwrap();
    let notices = restored.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, NoticeKind::PaidAfterFailed);
    assert_eq!(notices[0].attempt_id, Some(attempt_id));
}

/// The wallet's journal entry of `attempt_id`, its value rewritten to name `other`.
async fn journal_names(wallet: &mut Paykit, attempt_id: paykit_lib::AttemptId, other: &str) {
    let key = format!("j/{}", attempt_id.to_text());
    let bytes = wallet
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut entry: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    entry["d"]["attempt_id"] = serde_json::Value::String(other.to_owned());
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: key.into_bytes(),
            value: serde_json::to_vec(&entry).unwrap(),
        }])
        .await
        .unwrap();
}

#[tokio::test]
async fn test_notices_late_receipt_with_its_journal_entry_at_another_key_raises_none() {
    // The entry at the attempt's key names another attempt: a corrupt store, so the
    // receipt's step neither reads it as the payment's nor raises the notice from it.
    let w = World::new();
    let (mut wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    wallet.receive(None).await.unwrap();
    let other = paykit_lib::AttemptId::from_bytes([9; 16]).to_text();
    journal_names(&mut wallet, attempt_id, &other).await;
    receipt(&mut shop).await;
    let _ = wallet.receive(None).await;
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_notices_acknowledged_with_its_journal_entry_at_another_key_refused() {
    let w = World::new();
    let (mut wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    receipt(&mut shop).await;
    wallet.receive(None).await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1);
    let other = paykit_lib::AttemptId::from_bytes([9; 16]).to_text();
    journal_names(&mut wallet, attempt_id, &other).await;
    assert!(wallet.acknowledge_notice(&notices[0].id).await.is_err());
    assert_eq!(wallet.notices().await.unwrap(), notices);
}

#[tokio::test]
async fn test_notices_late_receipt_written_later_raised_by_an_own_app_from_its_kept_view() {
    // Alice's second wallet keeps the claim's view showing the attempt failed, and holds no
    // journal entry. The service deletes the claim; bob's receipt then reaches the second
    // wallet, whose step and first later write are refused. Its later write tells the user
    // from the kept view, as its first read would have.
    let w = World::new();
    let mut other = w.wallet("second").await;
    let (_wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    other.receive(None).await.unwrap();
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    // The client's own read first takes what is due at the switch past the window.
    other.app_mut().receive(None).await.unwrap();
    receipt(&mut shop).await;
    refuse_writes(&w, 2);
    other.receive(None).await.unwrap();
    assert!(other.notices().await.unwrap().is_empty());
    other.receive(None).await.unwrap();
    assert!(other.requests().await.unwrap()[0].receipt.is_some());
    let notices = other.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, NoticeKind::PaidAfterFailed);
    assert_eq!(notices[0].attempt_id, Some(attempt_id));
    // Raised once: a later receive raises it no more.
    other.receive(None).await.unwrap();
    assert_eq!(other.notices().await.unwrap(), notices);
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
async fn test_notices_late_receipt_raised_by_an_own_app_after_a_crossing_cancellation() {
    // Alice's second wallet holds the payer's record, not the payment's journal entry.
    // Bob's cancellation, sent before he saw the proof, reaches it after the proof and is
    // placed in the record; the first receipt past the claim's window still tells the user
    // the payment marked failed was paid.
    let w = World::new();
    let mut other = w.wallet("second").await;
    let (_wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    other.receive(None).await.unwrap();
    let request = other.requests().await.unwrap().remove(0).request;
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[other.app_id()],
        "paykit.payment_request_cancellation",
        vec![request.request_hash],
        serde_json::Map::new(),
    );
    other.receive(None).await.unwrap();
    other.resume_payments().await.unwrap();
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    receipt(&mut shop).await;
    other.receive(None).await.unwrap();
    let notices = other.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, NoticeKind::PaidAfterFailed);
    assert_eq!(notices[0].attempt_id, Some(attempt_id));
}

/// Bob's receipt of the proof `other`'s record holds, signed anew, entered for `targets`.
async fn receipt_again(
    w: &World,
    other: &Paykit,
    attempt_id: paykit_lib::AttemptId,
    targets: &[[u8; 16]],
) {
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
        targets,
        "paykit.payment_receipt",
        vec![record.proof.unwrap().hash(), view.request.request_hash],
        body,
    );
}

#[tokio::test]
async fn test_notices_later_receipt_raises_none_where_the_record_holds_one() {
    // Two own apps without the journal entry hold the failure in their claim views. The
    // first receipt past the window reaches one, which tells its user and records it; a
    // later receipt of the same proof at the other finds a receipt in the record already,
    // and raises nothing.
    let w = World::new();
    let mut first = w.wallet("second").await;
    let mut later = w.wallet("third").await;
    let (_wallet, _shop, attempt_id) = paid_after_failed(&w).await;
    first.receive(None).await.unwrap();
    later.receive(None).await.unwrap();
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    receipt_again(&w, &first, attempt_id, &[first.app_id()]).await;
    first.receive(None).await.unwrap();
    assert_eq!(first.notices().await.unwrap().len(), 1);
    later.receive(None).await.unwrap();
    receipt_again(&w, &later, attempt_id, &[later.app_id()]).await;
    later.receive(None).await.unwrap();
    assert!(later.notices().await.unwrap().is_empty());
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

#[tokio::test]
async fn test_notices_receipt_past_the_deadline_within_the_claim_settles_it_and_raises_none() {
    // Past the request's deadline, a day after it was sent, but within the 30 days the claim
    // is kept after it: the claim is still there, so the receipt settles it.
    let w = World::new();
    let (mut wallet, mut shop, _) = paid_after_failed(&w).await;
    w.clock.advance_ms(2 * DAY_MS);
    wallet.receive(None).await.unwrap();
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
async fn test_notices_late_receipt_raises_none_at_an_app_without_the_claim_or_the_payment() {
    // An own app that joins once the claim is gone holds no view of it, and the switch no
    // longer holds it: nothing stable tells it the payment was marked failed, since the
    // record's `status` is informational (Paykit wire 5.3). It records the receipt and
    // raises nothing; the app that made the payment raises the notice from its journal.
    let w = World::new();
    let (mut wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    let mut late = w.wallet("late").await;
    late.receive(None).await.unwrap();
    receipt(&mut shop).await;
    late.receive(None).await.unwrap();
    assert!(late.notices().await.unwrap().is_empty());
    let view = late.requests().await.unwrap().remove(0);
    assert!(view.receipt.is_some());
    wallet.receive(None).await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].attempt_id, Some(attempt_id));
}

#[tokio::test]
async fn test_notices_receipt_on_a_claim_settled_already_ends_the_wait() {
    // Another own app takes the receipt first and settles the claim; the wallet that made
    // the payment then takes it on a SETTLED claim: it waits no more, and its entry is
    // pruned past the claim's window.
    let w = World::new();
    let mut other = w.wallet("second").await;
    let (mut wallet, mut shop, _) = paid_after_failed(&w).await;
    other.receive(None).await.unwrap();
    receipt(&mut shop).await;
    other.receive(None).await.unwrap();
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let claim = other
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Settled);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_notices_receipt_at_exactly_the_claims_window_end_counts_it_gone() {
    // At the claim's deadline plus the 30 days it is kept after it, the switch may delete it:
    // the receipt counts the claim gone, and raises the notice.
    let w = World::new();
    let (mut wallet, mut shop, _) = paid_after_failed(&w).await;
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let deadline = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap()
        .view
        .deadline
        .unwrap();
    receipt(&mut shop).await;
    w.clock
        .set_ms(deadline + pubky_switch_core::claims::RECONCILIATION_MS);
    wallet.receive(None).await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, NoticeKind::PaidAfterFailed);
}

#[tokio::test]
async fn test_notices_late_receipt_held_while_its_record_awaits_its_key() {
    // The wallet that made the payment holds its record unopened after an epoch rotation:
    // the phone wrote it again under a key the wallet doesn't hold, though it holds a later
    // one. The late receipt is held, raising nothing; once the key comes it is taken once,
    // with one notice, and the payment waits no more.
    let w = World::new();
    let mut phone = w.wallet("phone").await;
    let (mut wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    let request = paykit_sdk::RequestRef {
        payee: w.bob_id(),
        request_hash: wallet.payments().await.unwrap()[0].request_hash.unwrap(),
    };
    phone.receive(None).await.unwrap();
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
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
    wallet.receive(None).await.unwrap();
    let key = wallet
        .app()
        .record_key(&crate::outcome_record::payer_record(&request))
        .unwrap();
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
    receipt(&mut shop).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, NoticeKind::PaidAfterFailed);
    assert_eq!(notices[0].attempt_id, Some(attempt_id));
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["receipt"].is_null(), "{value}");
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    wallet.resume_payments().await.unwrap();
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_notices_acknowledged_late_receipt_raises_none_again_after_a_restore() {
    // The host acknowledged "marked failed, was paid" before the backup. After the restore
    // the payee's receipt comes again: the acknowledged notice is restored with its
    // payment, so it is not raised again.
    let w = World::new();
    let (mut wallet, mut shop, attempt_id) = paid_after_failed(&w).await;
    w.clock.advance_ms(PAST_THE_CLAIM_MS);
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    receipt(&mut shop).await;
    wallet.receive(None).await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    let mut app = crate::stored::restored(&w, wallet).await;
    receipt_again(&w, &app, attempt_id, &[app.app_id()]).await;
    app.receive(None).await.unwrap();
    app.receive(None).await.unwrap();
    assert!(app.notices().await.unwrap().is_empty());
}
