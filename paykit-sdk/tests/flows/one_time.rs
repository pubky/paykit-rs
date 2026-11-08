//! One-time requests (Paykit spec 8.1): sent, received, accepted, paid, proven and
//! receipted; rejected and cancelled from each side.

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, Refusal, RequestStatus};
use pubky_switch_client::clock::Clock;

use crate::common::{draft, request_from, World, HOUR_MS};

#[tokio::test]
async fn test_one_time_request_paid_proven_and_receipted() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("0.00025", w.clock.now_ms())).await;

    // The payer's wallet receives the request and shows it open.
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(summary.applied.len(), 1, "{summary:?}");
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Open);
    assert_eq!(view.amount, crate::common::btc("0.00025"));

    // It pays: attempt, rail, done, acceptance and proof.
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );

    // The payee records the acceptance and the proof, verifies it and sends a receipt.
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert_eq!(sales[0].attempt_id, Some(attempt_id));
    let receipt = shop.confirm_payment(&sales[0].record).await.unwrap();
    assert!(receipt.is_some());
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Receipted
    );

    // The payer stores the receipt.
    wallet.receive(None).await.unwrap();
    let held = wallet.request(&request).await.unwrap();
    assert_eq!(held.status, RequestStatus::Paid);
}

#[tokio::test]
async fn test_one_time_payer_rejects_and_the_payee_sees_it() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();

    wallet
        .reject_request(&request, Some("user_rejected".into()))
        .await
        .unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Rejected
    );
    // A rejected request isn't paid, and isn't rejected twice.
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Rejected));
    let again = wallet.reject_request(&request, None).await.unwrap_err();
    assert_eq!(again.refusal(), Some(&Refusal::Rejected));

    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Rejected
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_payer_cancels_and_the_payee_sees_it() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();

    wallet.cancel_request(&request, None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Cancelled
    );
}

#[tokio::test]
async fn test_one_time_payee_cancels_and_the_payer_makes_no_attempt() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();

    let record = shop.payee_activity().await.unwrap()[0].record;
    shop.cancel_sent_request(&record, None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Cancelled
    );
    let twice = shop.cancel_sent_request(&record, None).await.unwrap_err();
    assert_eq!(twice.refusal(), Some(&Refusal::Cancelled));

    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_rejection_refused_while_a_payment_is_in_progress() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();

    w.rail.next(Behaviour::Hang);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let PayOutcome::Pending { attempt_id } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::BeingPaidHere
    );
    let refused = wallet.reject_request(&request, None).await.unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::PaymentInProgress));

    // The payment settles late; resuming reports it, once.
    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
}

#[tokio::test]
async fn test_one_time_expired_request_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut short = draft("1", w.clock.now_ms());
    short.proposal_expires_at_ms = Some(w.clock.now_ms() + HOUR_MS);
    let request = request_from(&mut shop, w.alice_id(), short).await;
    wallet.receive(None).await.unwrap();

    // Deadlines are read on the service's clock (Paykit spec 7.1): the wallet learns its
    // time from the next answer it verifies.
    w.clock.advance_ms(2 * HOUR_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Expired
    );
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::DeadlinePassed));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_one_time_request_within_its_expiry_is_paid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut short = draft("1", w.clock.now_ms());
    short.proposal_expires_at_ms = Some(w.clock.now_ms() + HOUR_MS);
    let request = request_from(&mut shop, w.alice_id(), short).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(HOUR_MS / 2);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_one_time_failed_payment_writes_failed_and_may_be_paid_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();

    w.rail.next(Behaviour::Fail);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Failed { .. }), "{outcome:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    let again = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(again, PayOutcome::Paid { .. }), "{again:?}");
}

#[tokio::test]
async fn test_one_time_transfer_of_another_amount_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut fixed = draft("1", w.clock.now_ms());
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-lightning-bolt11"),
        paykit_lib::PaymentEndpointPayload::new("lnbc?amount=2").unwrap(),
    );
    fixed.destination = paykit_lib::bodies::RequestDestination::Endpoints(endpoints);
    let request = request_from(&mut shop, w.alice_id(), fixed).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Transfer(_))),
        "{refused:?}"
    );
    assert!(wallet.payments().await.unwrap().is_empty());
}
