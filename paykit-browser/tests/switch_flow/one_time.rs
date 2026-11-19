//! Paying a one-time Payment Request (Paykit spec 8.1) through the real service: sent, read
//! open, paid, proven and receipted; and refused by the payment rail, then paid again.

use paykit_browser::clock::JsClock;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    MessageDelivery, PayOptions, PayOutcome, PayeeStatus, Paykit, ReceiptsOf, RequestStatus, Stage,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_core::claims::Outcome;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::{draft, invited_world, stored_request, DAY_MS, PAYEE, PAYER};

#[wasm_bindgen_test]
async fn test_switch_flow_request_paid_proven_and_receipted() {
    let (w, mut wallet, mut shop) = invited_world().await;

    // The payee's shop sends a one-time Payment Request, which the payer's switch accepts
    // at once: the wallet invited the payee.
    let sent_draft = draft("0.00025", JsClock.now_ms() + DAY_MS);
    let sent = shop
        .send_request(w.id(PAYER), sent_draft.clone())
        .await
        .unwrap();
    assert!(
        matches!(sent.delivery, MessageDelivery::Accepted { .. }),
        "{:?}",
        sent.delivery
    );
    let request = sent.request.expect("the request is recorded");

    // The payer's wallet receives it, once, and shows it open, as the payee sent it.
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(summary.applied, [sent.msg_id], "{summary:?}");
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Open);
    assert_eq!(view.request, request);
    assert_eq!(view.amount, sent_draft.amount);
    assert_eq!(
        view.payment_reference,
        sent_draft.payment_reference.as_str()
    );
    assert_eq!(
        view.proposal_expires_at_ms,
        sent_draft.proposal_expires_at_ms
    );
    assert!(!view.recurring);
    // Its activity record stores the payee's signed request, Payment Endpoint included.
    let stored = stored_request(&wallet, &view).await;
    assert_eq!(stored.amount, sent_draft.amount);
    assert_eq!(stored.destination, sent_draft.destination);
    assert_eq!(
        stored.proposal_expires_at,
        sent_draft.proposal_expires_at_ms
    );

    // The payer's wallet pays: attempt, rail, acceptance and proof.
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );

    // The payee's shop receives the acceptance and the Payment Proof, verifies the proof and
    // sends a Payment Receipt.
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert_eq!(sales[0].attempt_id, Some(attempt_id));
    let receipt_msg_id = shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .expect("the proof verifies, so a receipt is sent");
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Receipted
    );

    // The payer's wallet receives the receipt and stores it with the request it pays.
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Paid);
    let receipt = view.receipt.expect("the wallet stores the receipt");
    assert_eq!(receipt.msg_id, receipt_msg_id);
    assert_eq!(receipt.attempt_id, attempt_id);
    assert_eq!(receipt.payee, w.id(PAYEE));
    assert_eq!(receipt.request_hash, Some(request.request_hash));
    assert_eq!(receipt.payment_reference.as_deref(), Some("order-1"));
    let listed = wallet.receipts(ReceiptsOf::All).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].msg_id, receipt_msg_id);
}

#[wasm_bindgen_test]
async fn test_switch_flow_rail_refusal_fails_and_may_be_paid_again() {
    // A refusal says nothing was and nothing will be submitted (Paykit spec 7.2, "Failed
    // means definitively not paid"): the attempt is written `failed` at the service, a resume
    // repeats nothing, and a second payment goes ahead.
    let (w, mut wallet, mut shop) = invited_world().await;
    let sent = shop
        .send_request(w.id(PAYER), draft("1", JsClock.now_ms() + DAY_MS))
        .await
        .unwrap();
    let request = sent.request.expect("the request is recorded");
    wallet.receive(None).await.unwrap();

    w.rail.next(Behaviour::Refuse);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Failed { .. }), "{outcome:?}");
    assert_written_failed(&mut wallet).await;
    assert!(wallet.resume_payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );

    let again = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = again else {
        panic!("{again:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
}

/// The wallet's one payment is written `failed` (Paykit spec 7.2): its journal entry is
/// finished, and the claim the service holds shows its attempt failed.
async fn assert_written_failed(wallet: &mut Paykit) {
    let mut payments = wallet.payments().await.unwrap();
    assert_eq!(payments.len(), 1, "{payments:?}");
    let payment = payments.remove(0);
    assert_eq!(payment.stage, Stage::Finished);
    let claim = wallet
        .app_mut()
        .read_claim(&payment.obligation)
        .await
        .unwrap()
        .expect("the claim is read");
    assert_eq!(claim.view.attempts[0].outcome, Outcome::Failed);
}
