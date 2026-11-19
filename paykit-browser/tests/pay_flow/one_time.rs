//! Paying a one-time Payment Request (Paykit spec 8.1): paid, proven and receipted; refused
//! or failed by the payment rail, and then paid again; and refused once its expiry has
//! passed.

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    PayOptions, PayOutcome, PayeeStatus, Paykit, ReceiptsOf, Refusal, RequestStatus, Stage,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_core::claims::Outcome;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::world::{draft, send, World, DAY_MS, HOUR_MS};

#[wasm_bindgen_test]
async fn test_pay_flow_request_paid_proven_and_receipted() {
    let w = World::new();
    let mut wallet = w.wallet().await;
    let mut shop = w.shop().await;
    let request = send(
        &mut shop,
        w.payer_id(),
        draft("0.00025", w.clock.now_ms() + DAY_MS),
    )
    .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
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
    assert_eq!(receipt.payee, w.payee_id());
    assert_eq!(receipt.request_hash, Some(request.request_hash));
    assert_eq!(receipt.payment_reference.as_deref(), Some("order-1"));
    let listed = wallet.receipts(ReceiptsOf::All).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].msg_id, receipt_msg_id);
}

#[wasm_bindgen_test]
async fn test_pay_flow_rail_refusal_fails_and_may_be_paid_again() {
    // A refusal says nothing was and nothing will be submitted (Paykit spec 7.2, "Failed
    // means definitively not paid"): the attempt is written `failed`, a resume repeats
    // nothing, and a second payment goes ahead.
    let w = World::new();
    let mut wallet = w.wallet().await;
    let mut shop = w.shop().await;
    let request = send(
        &mut shop,
        w.payer_id(),
        draft("1", w.clock.now_ms() + DAY_MS),
    )
    .await;
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
}

#[wasm_bindgen_test]
async fn test_pay_flow_failed_payment_may_be_paid_again() {
    let w = World::new();
    let mut wallet = w.wallet().await;
    let mut shop = w.shop().await;
    let request = send(
        &mut shop,
        w.payer_id(),
        draft("1", w.clock.now_ms() + DAY_MS),
    )
    .await;
    wallet.receive(None).await.unwrap();

    // The rail fails the payment: no money moved, the attempt is written `failed`, a resume
    // repeats nothing, and the request stays open.
    w.rail.next(Behaviour::Fail);
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
}

#[wasm_bindgen_test]
async fn test_pay_flow_expired_request_refused() {
    let w = World::new();
    let mut wallet = w.wallet().await;
    let mut shop = w.shop().await;
    let request = send(
        &mut shop,
        w.payer_id(),
        draft("1", w.clock.now_ms() + HOUR_MS),
    )
    .await;
    wallet.receive(None).await.unwrap();

    // Deadlines are read on the service's clock (Paykit spec 7.1): the wallet learns its time
    // from the next answer it verifies.
    w.clock.advance_ms(2 * HOUR_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Expired
    );
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::DeadlinePassed));
    assert!(w.rail.all_executions().is_empty());
}

/// The wallet's one payment is written `failed` (Paykit spec 7.2): its journal entry is
/// finished, and the claim shows its attempt failed.
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
