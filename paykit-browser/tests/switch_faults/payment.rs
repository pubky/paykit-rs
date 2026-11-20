//! Faults on the payer's operation batches while it pays a one-time Payment Request through
//! the real service (Paykit spec 7.2): the answer to a batch the service applied is lost, or
//! the batch is refused before it reaches the service. Either way the payer pays once.
//!
//! The steps and the outcomes they check follow paykit-sdk's native flow tests, whose scripted
//! switch fails the same batch:
//! `test_one_time_attempt_kept_then_applied_writes_failed_and_pays_once`
//! (`paykit-sdk/tests/flows/one_time.rs`) for the attempt's batch, and
//! `test_unrequested_paid_with_its_outcome_unwritten_is_no_error`
//! (`paykit-sdk/tests/flows/unrequested.rs`) for the outcome's batch, after the rail paid.
//!
//! There the switch fails to connect, which the client knows sent nothing. A browser's
//! `fetch` reports a connection that broke before an answer as one opaque error, whether the
//! request left or not, so `FetchTransport` maps both faults to an outcome the client can't
//! know (`src/fetch.rs`): the client keeps the batch as sent, even when the service never saw
//! it, and sends the same bytes again. The SDK's outcomes are still the native ones, and the
//! rig's counts show what reached the service.

use paykit_browser::clock::JsClock;
use paykit_lib::AttemptId;
use paykit_sdk::{
    PayOptions, PayOutcome, PayeeStatus, Paykit, Refusal, RequestRef, RequestStatus, Stage,
};
use pubky_switch_client::app::KeptBatchStatus;
use pubky_switch_client::clock::Clock;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::faults::{with_fault, Fault, FaultKind};
use crate::switch_rig::{account_path, path_hits, PathHits};
use crate::switch_world::{SwitchWorld, PAYEE, PAYER};
use crate::world::{draft, send, DAY_MS};

/// A new world on the service, with the payer's wallet and the payee's shop joined, the
/// payee invited by the payer, and a one-time Payment Request from the shop open at the
/// wallet. Returns the world, the wallet, the shop and the request.
async fn open_request() -> (SwitchWorld, Paykit, Paykit, RequestRef) {
    let w = SwitchWorld::enrol(&[PAYER, PAYEE], &[(PAYER, "wallet"), (PAYEE, "shop")]).await;
    let mut wallet = w.join("payer:wallet").await;
    let mut shop = w.join("payee:shop").await;
    wallet.app_mut().invite(&w.id(PAYEE)).await.unwrap();
    let request = send(
        &mut shop,
        w.id(PAYER),
        draft("1", JsClock.now_ms() + DAY_MS),
    )
    .await;
    wallet.receive(None).await.unwrap();
    (w, wallet, shop, request)
}

/// The raw path of the payer's operation batches at the service, `POST /v1/{id}/ops`
/// (Switch wire 6.4).
fn payer_ops(w: &SwitchWorld) -> String {
    account_path(&w.id(PAYER), "ops")
}

/// A fault of `kind` on the payer's operation batches at `ops`, once `after` of them went
/// through.
fn ops_fault(ops: &str, kind: FaultKind, after: u8) -> Fault {
    Fault {
        method: "POST",
        path: ops.to_owned(),
        kind,
        after,
    }
}

/// The `POST` requests to `ops` the rig counted since `before`: how many it sent on to the
/// service, and how many a `refuse` fault kept from the service.
async fn ops_hits_since(ops: &str, before: PathHits) -> (u64, u64) {
    let hits = path_hits(ops, Some("POST")).await.since(before);
    (hits.forwarded, hits.refused)
}

/// How far the wallet's one payment went, as its journal entry shows.
async fn journaled(wallet: &Paykit) -> Stage {
    let payments = wallet.payments().await.unwrap();
    assert_eq!(payments.len(), 1, "{payments:?}");
    payments[0].stage
}

/// The shop receives the payment's Payment Proof: it holds one sale, for `attempt_id`.
async fn one_sale(shop: &mut Paykit, attempt_id: AttemptId) {
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert_eq!(sales[0].attempt_id, Some(attempt_id));
}

/// The attempt's batch, the payer's first, meets `kind`. The call fails with the batch kept
/// and nothing paid. The next flush sends the same batch again and the service applies it,
/// or answers it as applied: the attempt is open with no payment started, so a second
/// payment is refused while it is in progress, a resume writes `failed`, and the next payment
/// pays once.
async fn attempt_faulted_then_paid_once(kind: FaultKind) {
    let (w, mut wallet, mut shop, request) = open_request().await;
    let ops = payer_ops(&w);
    let before = path_hits(&ops, Some("POST")).await;
    let fault = ops_fault(&ops, kind, 0);
    let kept = with_fault(&fault, wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome
        .unwrap_err();
    let batch_id = kept.kept_batch().unwrap_or_else(|| panic!("{kept:?}"));
    // The batch reached the rig once, and was sent on to the service unless refused.
    let refused = u64::from(kind == FaultKind::Refuse);
    assert_eq!(ops_hits_since(&ops, before).await, (1 - refused, refused));
    // Lost or refused, the client keeps it as sent: the browser can't say which.
    let status = wallet.app().kept_batch(batch_id).await.unwrap();
    assert!(
        matches!(
            status,
            Some(KeptBatchStatus::Kept {
                sent_at_ms: Some(_),
                ..
            })
        ),
        "{status:?}"
    );
    assert_eq!(journaled(&wallet).await, Stage::Recorded);
    assert!(w.rail.all_executions().is_empty());

    // The flush sends the same batch once more, and it is applied.
    wallet.flush().await.unwrap();
    let status = wallet.app().kept_batch(batch_id).await.unwrap();
    assert!(
        matches!(status, Some(KeptBatchStatus::Applied { .. })),
        "{status:?}"
    );
    assert_eq!(ops_hits_since(&ops, before).await, (2 - refused, refused));
    let busy = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert_eq!(
        busy.refusal(),
        Some(&Refusal::PaymentInProgress),
        "{busy:?}"
    );
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Failed { .. }]),
        "{resumed:?}"
    );
    assert!(w.rail.all_executions().is_empty());

    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = paid else {
        panic!("{paid:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
    one_sale(&mut shop, attempt_id).await;
}

#[wasm_bindgen_test]
async fn test_switch_faults_attempt_answer_lost_writes_failed_and_pays_once() {
    attempt_faulted_then_paid_once(FaultKind::Lose).await;
}

#[wasm_bindgen_test]
async fn test_switch_faults_attempt_refused_writes_failed_and_pays_once() {
    attempt_faulted_then_paid_once(FaultKind::Refuse).await;
}

#[wasm_bindgen_test]
async fn test_switch_faults_outcome_answer_lost_resumed_without_paying_again() {
    // The attempt's batch goes through and the rail pays; the answer to the outcome's batch
    // (`done`, the payer's record, the acceptance and the proof) is lost after the service
    // applied it. Money moved, so the call reports it paid with its outcome unwritten, never
    // an error a host would answer with a second payment. A resume sends the same batch
    // again, which settles the outcome and sends the proof, paying nothing again (Paykit
    // spec 7.2 steps 3 and 4).
    let (w, mut wallet, mut shop, request) = open_request().await;
    let ops = payer_ops(&w);
    let before = path_hits(&ops, Some("POST")).await;
    let fault = ops_fault(&ops, FaultKind::Lose, 1);
    let outcome = with_fault(&fault, wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome
        .unwrap();
    let PayOutcome::PaidOutcomeUnwritten { attempt_id, .. } = outcome else {
        panic!("money moved, so the call reports it paid: {outcome:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
    // The attempt's batch and the outcome's, each sent on to the service once.
    assert_eq!(ops_hits_since(&ops, before).await, (2, 0));
    assert_eq!(journaled(&wallet).await, Stage::RailPaid);

    wallet.resume_payments().await.unwrap();
    assert_eq!(journaled(&wallet).await, Stage::Finished);
    assert_eq!(ops_hits_since(&ops, before).await, (3, 0));
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
    one_sale(&mut shop, attempt_id).await;
}
