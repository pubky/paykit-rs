//! One-time requests (Paykit spec 8.1): sent, received, accepted, paid, proven and
//! receipted; rejected and cancelled from each side.

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, Refusal, RequestStatus};
use pubky_switch_client::clock::Clock;

use crate::common::{draft, request_from, FailingRecordReads, World, HOUR_MS};

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

#[tokio::test]
async fn test_one_time_payee_activity_store_failure_is_an_error() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let failing = std::sync::Arc::new(FailingRecordReads::default());
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
            paykit_sdk::PaykitConfig::default(),
            std::sync::Arc::clone(&failing)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 1);
    // The collection lists, but its record can't be read again: the store failure is the
    // answer, never a list that silently leaves the record out.
    failing.arm();
    let error = shop.payee_activity().await.unwrap_err();
    assert!(
        matches!(
            error,
            paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::Store(_))
        ),
        "{error:?}"
    );
}

/// A one-time request from bob's `shop` to alice, signed and not delivered, with its hash.
fn unsent_request(w: &World) -> (pubky_switch_core::message::MessagePlaintext, [u8; 32]) {
    let body = serde_json::json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "order-1",
        "proposal_expires_at": null,
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
    });
    let serde_json::Value::Object(body) = body else {
        unreachable!()
    };
    let plaintext = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_request",
        vec![],
        body,
    );
    let hash = plaintext.hash();
    (plaintext, hash)
}

/// Bob's cancellation of the request `hash`, entered for alice's apps `targets`.
fn cancel_to(w: &World, targets: &[[u8; 16]], hash: [u8; 32]) -> String {
    let body = paykit_lib::bodies::RequestReply {
        reason: None,
        label: None,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        targets,
        "paykit.payment_request_cancellation",
        vec![hash],
        body,
    )
}

#[tokio::test]
async fn test_one_time_cancellation_before_its_request_keeps_it_cancelled() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    // The request reaches the wallet unopenable, so it holds no record of it; then bob's
    // cancellation of it arrives, and the cursor moves past it.
    let (request, hash) = unsent_request(&w);
    let cancellation = cancel_to(&w, &[wallet.app_id()], hash);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.applied.contains(&cancellation), "{got:?}");
    // The request is re-sent and opened: it is cancelled, and never payable (Paykit spec
    // 8.1).
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &request);
    let got = wallet.receive(None).await.unwrap();
    assert_eq!(got.applied.len(), 1, "{got:?}");
    let request = paykit_sdk::RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
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
async fn test_one_time_cancellation_kept_applies_to_another_wallets_record() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let _shop = w.shop().await;
    // The wallet gets bob's cancellation before the request; the phone gets only the
    // request, and writes its record without the cancellation.
    let (request, hash) = unsent_request(&w);
    cancel_to(&w, &[wallet.app_id()], hash);
    wallet.receive(None).await.unwrap();
    w.deliver(&w.bob, "shop", &w.alice, &[phone.app_id()], &request);
    phone.receive(None).await.unwrap();
    let request = paykit_sdk::RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    assert_eq!(
        phone.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    // The wallet pays from the phone's record: the cancellation it kept goes into the record
    // first, and the payment is refused.
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    assert!(w.rail.all_executions().is_empty());
    phone.receive(None).await.unwrap();
    assert_eq!(
        phone.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
}

#[tokio::test]
async fn test_one_time_receipt_naming_another_proof_leaves_the_proof_recordable() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    // Bob's receipt for the attempt names a proof alice's record doesn't hold (Paykit wire
    // 4.11): it settles the attempt, and isn't stored beside a proof it doesn't confirm.
    let body = paykit_lib::bodies::PaymentReceipt {
        attempt_id,
        billing_period: None,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_receipt",
        vec![[7; 32], request.request_hash],
        body,
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
    // The payment completes: its proof is still recorded, in a record readers take (Paykit
    // wire 5.3), and sent.
    w.rail.settle(&attempt_id, true);
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
    let key = wallet.request(&request).await.unwrap().record;
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(!value["proof"].is_null());
    assert!(value["receipt"].is_null());
}
